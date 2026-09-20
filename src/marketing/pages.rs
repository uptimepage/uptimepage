//! Page handlers (landing + branded 404). Landing renders into a
//! `OnceLock` at boot via [`warm`], so every request after that serves
//! the cached body + stable ETag with no askama work and no per-request
//! allocations beyond cheap `Bytes` / `HeaderValue` clones.

use std::sync::Arc;
use std::sync::OnceLock;

use askama::Template;
use askama_web::WebTemplate;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::HeaderValue;
use axum::http::StatusCode;
use axum::http::Uri;
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE, ETAG, IF_NONE_MATCH};
use axum::response::{IntoResponse, Redirect, Response};
use bytes::Bytes;

use crate::marketing::seo::{
    JsonLd, OpenGraph, json_ld_breadcrumb, json_ld_faqpage, json_ld_organization,
    json_ld_software_application, json_ld_software_source_code, json_ld_tech_article,
    json_ld_webpage, json_ld_website,
};
use crate::templates::filters;

use super::config::{BRAND, MarketingCfg};
use super::gallery;

pub(super) const HTML_CONTENT_TYPE: HeaderValue =
    HeaderValue::from_static("text/html; charset=utf-8");
pub(super) const TEXT_PLAIN: HeaderValue = HeaderValue::from_static("text/plain; charset=utf-8");
pub(super) const APPLICATION_XML: HeaderValue =
    HeaderValue::from_static("application/xml; charset=utf-8");

const PAGE_CACHE_CONTROL: HeaderValue =
    HeaderValue::from_static("public, max-age=300, stale-while-revalidate=86400");
const NOT_FOUND_CACHE_CONTROL: HeaderValue = HeaderValue::from_static("public, max-age=300");

/// Neighbours resolved here, wrapping, so no lightbox arrow can point at a
/// dead anchor.
struct ShotView {
    id: &'static str,
    src: String,
    alt: &'static str,
    caption: &'static str,
    width: u32,
    height: u32,
    prev_id: &'static str,
    next_id: &'static str,
}

fn shot_views() -> Vec<ShotView> {
    let n = gallery::SHOTS.len();
    gallery::SHOTS
        .iter()
        .enumerate()
        .map(|(i, s)| ShotView {
            id: s.id,
            src: crate::templates::assets::url(s.file),
            alt: s.alt,
            caption: s.caption,
            width: s.width,
            height: s.height,
            prev_id: gallery::SHOTS[(i + n - 1) % n].id,
            next_id: gallery::SHOTS[(i + 1) % n].id,
        })
        .collect()
}

#[derive(Template, WebTemplate)]
#[template(path = "marketing/landing.html")]
struct LandingPage {
    app_url: String,
    canonical_url: String,
    og: OpenGraph,
    org_json_ld: JsonLd,
    website_json_ld: JsonLd,
    software_json_ld: JsonLd,
    source_code_json_ld: JsonLd,
    faq_json_ld: JsonLd,
    version: &'static str,
    faqs: &'static [(&'static str, &'static str)],
    show_gallery: bool,
    shots: Vec<ShotView>,
    start_band_position: &'static str,
}

/// One source for the rendered FAQ and its `FAQPage` schema, so they can't drift.
const FAQS: &[(&str, &str)] = &[
    (
        "Can I use my own domain for the status page?",
        "Every org gets <code class=\"mk-chip\" translate=\"no\">your-org.uptimepage.dev</code> \
         out of the box. A custom CNAME (<code class=\"mk-chip\" translate=\"no\">status.yourcompany.com</code>) \
         is coming. Drop a line if you need it sooner.",
    ),
    (
        "What kinds of monitors are supported?",
        "HTTP/HTTPS, TCP port, DNS lookup, ICMP ping, cron-job heartbeat, TLS-certificate \
         and domain expiry, and a browser login flow that signs in for real and charts how \
         long each step takes. Per-monitor headers, basic-auth, bearer tokens, expected \
         status code, content-match, TLS verification, follow-redirects rules.",
    ),
    (
        "Where do alerts come from?",
        "Slack, Discord, Teams, Mattermost, Telegram, email, PagerDuty, ntfy, \
         Pushover, Gotify, WhatsApp, or any HTTPS webhook. Each monitor binds its own channels, so \
         a marketing-site flap doesn’t page on-call.",
    ),
    (
        "Can I export my data?",
        "Always. JSON export per monitor and incident. RSS for public incidents. \
         SVG badges you can drop in a README.",
    ),
];

#[derive(Template, WebTemplate)]
#[template(path = "marketing/not_found.html")]
pub struct NotFoundPage {
    pub canonical_url: String,
}

/// One landing render per process. The body is invariant after boot —
/// `app_url`, `canonical_origin`, version, JSON-LD all come from
/// startup config — so re-rendering and re-hashing per request would
/// burn ~80–150 µs for an identical response.
static LANDING_CACHED: OnceLock<CachedRender> = OnceLock::new();
static LANDING_MD: OnceLock<CachedRender> = OnceLock::new();
static NF_CACHED: OnceLock<CachedRender> = OnceLock::new();

#[derive(Clone)]
pub(crate) struct CachedRender {
    // `Bytes` + `HeaderValue` so the hot-path clone is an `Arc` bump,
    // not a heap copy of the rendered HTML.
    pub(crate) body: Bytes,
    pub(crate) etag: HeaderValue,
}

fn render_landing(cfg: &MarketingCfg) -> CachedRender {
    let canonical_url = cfg.canonical_origin.clone();
    let mut og = OpenGraph::default_for(
        &format!("{BRAND}: uptime monitoring and status pages in one"),
        &canonical_url,
        &cfg.canonical_origin,
    );
    og.description = "Hosted uptime monitoring for websites and APIs, with multi-region checks, team alerts, incidents, and public status pages. Start free; open source for control.".to_string();
    let page = LandingPage {
        app_url: cfg.app_url.clone(),
        canonical_url,
        org_json_ld: json_ld_organization(&cfg.canonical_origin),
        website_json_ld: json_ld_website(&cfg.canonical_origin),
        software_json_ld: json_ld_software_application(&cfg.canonical_origin),
        source_code_json_ld: json_ld_software_source_code(&cfg.canonical_origin),
        faq_json_ld: json_ld_faqpage(FAQS),
        og,
        version: env!("CARGO_PKG_VERSION"),
        faqs: FAQS,
        show_gallery: super::config::GALLERY_VISIBLE,
        shots: shot_views(),
        start_band_position: "band-url",
    };
    let body = page
        .render()
        .unwrap_or_else(|e| format!("<!-- landing render failed: {e} -->"));
    cached_render(body)
}

fn render_not_found(cfg: &MarketingCfg) -> CachedRender {
    let body = NotFoundPage {
        canonical_url: cfg.canonical_origin.clone(),
    }
    .render()
    .unwrap_or_else(|_| "Not Found".to_string());
    cached_render(body)
}

pub async fn landing(State(cfg): State<Arc<MarketingCfg>>, headers: HeaderMap) -> Response {
    let cached = LANDING_CACHED.get_or_init(|| render_landing(&cfg));
    let markdown = LANDING_MD.get_or_init(|| render_landing_markdown(&cfg));
    super::negotiate::serve(&headers, cached, markdown, &PAGE_CACHE_CONTROL)
}

/// The landing page has no Markdown source of its own — it is a template.
/// `llms.txt` is the authored Markdown statement of the same thing, so an
/// agent asking for Markdown at `/` gets the site index rather than a
/// stripped-down rendering of the hero.
fn render_landing_markdown(cfg: &MarketingCfg) -> CachedRender {
    let body = super::seo::llms_markdown(cfg);
    CachedRender {
        etag: body_etag(&String::from_utf8_lossy(&body)),
        body,
    }
}

/// Trailing-slash form of a real page, or `None` when there is nothing to
/// redirect to. Every route is registered bare, so `/blog/` 404s where `/blog`
/// serves. A leading `//` would make the browser read the target as
/// protocol-relative and leave the site, so it stays a 404.
fn slash_redirect(uri: &Uri) -> Option<String> {
    let path = uri.path();
    if !path.ends_with('/') {
        return None;
    }
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() || trimmed.starts_with("//") {
        return None;
    }
    Some(match uri.query() {
        Some(q) => format!("{trimmed}?{q}"),
        None => trimmed.to_string(),
    })
}

/// Router fallback: nothing matched at all, so a slash fixup is worth trying
/// before giving up.
pub async fn not_found(State(cfg): State<Arc<MarketingCfg>>, uri: Uri) -> Response {
    match slash_redirect(&uri) {
        Some(target) => Redirect::permanent(&target).into_response(),
        None => not_found_page(&cfg),
    }
}

/// The 404 itself. Handlers that matched a route but not a slug render this
/// directly — their path is already canonical, so there is nothing to fix up.
pub fn not_found_page(cfg: &MarketingCfg) -> Response {
    let cached = NF_CACHED.get_or_init(|| render_not_found(cfg));
    (
        StatusCode::NOT_FOUND,
        [
            (CONTENT_TYPE, HTML_CONTENT_TYPE),
            (CACHE_CONTROL, NOT_FOUND_CACHE_CONTROL),
            (ETAG, cached.etag.clone()),
        ],
        cached.body.clone(),
    )
        .into_response()
}

pub(crate) const ARCHITECTURE_PATH: &str = "/architecture";
pub(crate) const ARCHITECTURE_LASTMOD: &str = "2026-08-03";

/// The map's own data, shared with `assets/js/architecture/_data.js` so the
/// crawlable HTML and the interactive map cannot describe different systems.
const ARCH_DATA: &str = include_str!("../../assets/js/architecture/flows.json");

#[derive(serde::Deserialize)]
pub struct ArchColumn {
    pub id: String,
    pub label: String,
}

#[derive(serde::Deserialize)]
pub struct ArchNode {
    pub id: String,
    pub col: String,
    pub t: String,
    pub s: String,
}

#[derive(serde::Deserialize)]
pub struct ArchStep {
    pub f: String,
    pub t: String,
    pub h: String,
    pub d: String,
}

#[derive(serde::Deserialize)]
pub struct ArchFlow {
    pub id: String,
    pub name: String,
    pub desc: String,
    pub steps: Vec<ArchStep>,
}

#[derive(serde::Deserialize)]
struct ArchData {
    columns: Vec<ArchColumn>,
    nodes: Vec<ArchNode>,
    flows: Vec<ArchFlow>,
}

/// A column with its nodes already grouped, so the template does not filter.
pub struct ArchColumnView {
    pub id: String,
    pub label: String,
    pub nodes: Vec<ArchNode>,
}

static ARCH_PARSED: OnceLock<(Vec<ArchColumnView>, Vec<ArchFlow>)> = OnceLock::new();

fn arch_data() -> &'static (Vec<ArchColumnView>, Vec<ArchFlow>) {
    ARCH_PARSED.get_or_init(|| {
        let data: ArchData =
            serde_json::from_str(ARCH_DATA).expect("architecture flows.json must parse");
        let mut remaining = data.nodes;
        let mut columns = Vec::with_capacity(data.columns.len());
        for c in data.columns {
            let (mine, rest) = remaining.into_iter().partition(|n| n.col == c.id);
            remaining = rest;
            columns.push(ArchColumnView {
                id: c.id,
                label: c.label,
                nodes: mine,
            });
        }
        // A node in no column would silently never reach the page.
        assert!(
            remaining.is_empty(),
            "architecture nodes reference unknown columns: {:?}",
            remaining.iter().map(|n| &n.id).collect::<Vec<_>>()
        );
        (columns, data.flows)
    })
}

/// The map carries every flow; only these earn a written entry, because each
/// one teaches something the others do not.
const REFERENCE_FLOWS: &[&str] = &["sched-http", "agent", "incident", "status", "erasure"];

#[cfg(test)]
mod arch_tests {
    use super::*;

    #[test]
    fn every_node_lands_in_a_column_and_every_hop_resolves() {
        let (columns, flows) = arch_data();
        let ids: std::collections::HashSet<&str> = columns
            .iter()
            .flat_map(|c| c.nodes.iter().map(|n| n.id.as_str()))
            .collect();
        // A hop pointing at a node that is not rendered leaves the map with a
        // wire to nowhere and no handler bound.
        for f in flows {
            for s in &f.steps {
                assert!(
                    ids.contains(s.f.as_str()),
                    "{}: unknown from {:?}",
                    f.id,
                    s.f
                );
                assert!(ids.contains(s.t.as_str()), "{}: unknown to {:?}", f.id, s.t);
            }
        }
    }

    #[test]
    fn every_reference_flow_exists() {
        let (_, flows) = arch_data();
        for id in REFERENCE_FLOWS {
            assert!(
                flows.iter().any(|f| f.id == *id),
                "reference flow {id:?} is not in flows.json"
            );
        }
    }
}

#[derive(Template, WebTemplate)]
#[template(path = "marketing/architecture.html")]
struct ArchitecturePage {
    app_url: String,
    canonical_url: String,
    og: OpenGraph,
    breadcrumb_json_ld: JsonLd,
    article_json_ld: JsonLd,
    columns: &'static [ArchColumnView],
    flows: &'static [ArchFlow],
    reference: Vec<&'static ArchFlow>,
    version: &'static str,
}

static ARCH_CACHED: OnceLock<CachedRender> = OnceLock::new();

fn render_architecture(cfg: &MarketingCfg) -> CachedRender {
    let canonical_url = format!("{}{ARCHITECTURE_PATH}", cfg.canonical_origin);
    let title = format!("How {BRAND} is built: architecture and flows");
    let description = "An interactive map of Uptimepage, the open-source uptime monitor: click a runtime flow and watch it light up across every process, surface, service and store.";
    let mut og = OpenGraph::default_for(&title, &canonical_url, &cfg.canonical_origin);
    og.description = description.to_string();
    og.image = format!(
        "{}/static/marketing/og-architecture.png",
        cfg.canonical_origin
    );
    let (columns, flows) = arch_data();
    let reference = REFERENCE_FLOWS
        .iter()
        .filter_map(|id| flows.iter().find(|f| f.id == *id))
        .collect();
    let body = ArchitecturePage {
        app_url: cfg.app_url.clone(),
        canonical_url,
        og,
        breadcrumb_json_ld: json_ld_breadcrumb(
            &cfg.canonical_origin,
            "Architecture",
            ARCHITECTURE_PATH,
        ),
        // TechArticle, not WebPage: the page explains how the system works and
        // carries an author, which is what a citation needs.
        article_json_ld: json_ld_tech_article(
            &cfg.canonical_origin,
            ARCHITECTURE_PATH,
            &title,
            description,
            ARCHITECTURE_LASTMOD,
        ),
        columns,
        flows,
        reference,
        version: env!("CARGO_PKG_VERSION"),
    }
    .render()
    .unwrap_or_else(|e| format!("<!-- architecture render failed: {e} -->"));
    cached_render(body)
}

pub async fn architecture(State(cfg): State<Arc<MarketingCfg>>, headers: HeaderMap) -> Response {
    let cached = ARCH_CACHED.get_or_init(|| render_architecture(&cfg));
    serve_cached(&headers, cached, &PAGE_CACHE_CONTROL)
}

const PRICING_CREATED: &str = "2026-06-23";
const PRICING_LASTMOD: &str = "2026-09-12";

// Founding-claim figures shown on the pricing scarcity meter.
const FOUNDING_TOTAL: u32 = 1000;
const FOUNDING_CLAIMED: u32 = 713;

const PRICING_FAQS: &[(&str, &str)] = &[
    (
        "Is the $0 plan actually free?",
        "Yes. No card, no trial clock, no surprise bill. The product is open \
         source, so we are not staking the company on locking you in. Standard \
         stays at the limits you see here.",
    ),
    (
        "What happens when the founding spots run out?",
        "New accounts get Standard, and Pro starts at $9 with the founding limits \
         plus your own branding. If you claimed a founding spot you keep it at the \
         founding limits for as long as the account is open, at no cost. We do not \
         quietly downgrade you.",
    ),
    (
        "Can I self-host instead of using your servers?",
        "Yes, fully. Clone the repo and run <code class=\"mk-chip\" translate=\"no\">docker compose up</code>, \
         and you have the whole product with no plan limits. It is AGPL, and you do \
         not need an account with us.",
    ),
    (
        "Will you change the free limits later?",
        "We may adjust Standard for new signups as costs change. Anything already on \
         your account is grandfathered, and founding stays founding.",
    ),
    (
        "Do you sell or train on my data?",
        "No. Your monitors, incidents, and contacts are yours. We make money from the \
         paid plans and hosting, not from your data.",
    ),
];

#[derive(Template, WebTemplate)]
#[template(path = "marketing/pricing.html")]
struct PricingPage {
    app_url: String,
    checkout_open: bool,
    canonical_url: String,
    og: OpenGraph,
    breadcrumb_json_ld: JsonLd,
    software_json_ld: JsonLd,
    webpage_json_ld: JsonLd,
    faq_json_ld: JsonLd,
    faqs: &'static [(&'static str, &'static str)],
    founding_claimed: u32,
    founding_total: u32,
    founding_left: u32,
    founding_pct: u32,
    founding_pct_exact: u32,
    version: &'static str,
}

static PRICING_CACHED: OnceLock<CachedRender> = OnceLock::new();

fn render_pricing(cfg: &MarketingCfg) -> CachedRender {
    let canonical_url = format!("{}/pricing", cfg.canonical_origin);
    let mut og = OpenGraph::default_for(
        &format!("Uptime Monitoring Pricing: Free & Pro | {BRAND}"),
        &canonical_url,
        &cfg.canonical_origin,
    );
    og.description = "Uptimepage pricing: a free Standard plan with no card, founding \
         free for the first 1,000 accounts, and Pro for teams in production. \
         Self-host AGPL, no limits."
        .to_string();
    let page = PricingPage {
        app_url: cfg.app_url.clone(),
        checkout_open: cfg.checkout_open,
        breadcrumb_json_ld: json_ld_breadcrumb(&cfg.canonical_origin, "Pricing", "/pricing"),
        software_json_ld: json_ld_software_application(&cfg.canonical_origin),
        webpage_json_ld: json_ld_webpage(
            &cfg.canonical_origin,
            "/pricing",
            "Pricing",
            PRICING_CREATED,
            PRICING_LASTMOD,
            true,
        ),
        faq_json_ld: json_ld_faqpage(PRICING_FAQS),
        faqs: PRICING_FAQS,
        canonical_url,
        og,
        founding_claimed: FOUNDING_CLAIMED,
        founding_total: FOUNDING_TOTAL,
        founding_left: FOUNDING_TOTAL.saturating_sub(FOUNDING_CLAIMED),
        // Nearest 5 so it maps to a `fnd-meter__fill--wN` class; the
        // marketing CSP blocks the inline width style attribute.
        founding_pct: {
            let raw = (FOUNDING_CLAIMED * 100)
                .checked_div(FOUNDING_TOTAL)
                .unwrap_or(0);
            ((raw + 2) / 5 * 5).min(100)
        },
        // Shown as the gauge readout, so it must match the exact claimed/total.
        founding_pct_exact: (FOUNDING_CLAIMED * 100 + FOUNDING_TOTAL / 2)
            .checked_div(FOUNDING_TOTAL)
            .unwrap_or(0)
            .min(100),
        version: env!("CARGO_PKG_VERSION"),
    };
    let body = page
        .render()
        .unwrap_or_else(|e| format!("<!-- pricing render failed: {e} -->"));
    cached_render(body)
}

pub async fn pricing(State(cfg): State<Arc<MarketingCfg>>, headers: HeaderMap) -> Response {
    let cached = PRICING_CACHED.get_or_init(|| render_pricing(&cfg));
    serve_cached(&headers, cached, &PAGE_CACHE_CONTROL)
}

pub(crate) fn warm(cfg: &MarketingCfg) {
    LANDING_CACHED.get_or_init(|| render_landing(cfg));
    LANDING_MD.get_or_init(|| render_landing_markdown(cfg));
    PRICING_CACHED.get_or_init(|| render_pricing(cfg));
    ARCH_CACHED.get_or_init(|| render_architecture(cfg));
    NF_CACHED.get_or_init(|| render_not_found(cfg));
}

/// 200 + body OR 304 when the client's `If-None-Match` matches. ETag
/// is a SHA-256 prefix of the rendered HTML so identical builds serve a
/// stable token — what makes a CDN's revalidation actually save bytes.
pub(crate) fn serve_cached(
    headers: &HeaderMap,
    cached: &CachedRender,
    cache_control: &HeaderValue,
) -> Response {
    if headers.get(IF_NONE_MATCH).is_some_and(|v| v == cached.etag) {
        return (
            StatusCode::NOT_MODIFIED,
            [
                (ETAG, cached.etag.clone()),
                (CACHE_CONTROL, cache_control.clone()),
            ],
        )
            .into_response();
    }
    (
        StatusCode::OK,
        [
            (CONTENT_TYPE, HTML_CONTENT_TYPE),
            (CACHE_CONTROL, cache_control.clone()),
            (ETAG, cached.etag.clone()),
        ],
        cached.body.clone(),
    )
        .into_response()
}

pub(crate) fn cached_render(body: String) -> CachedRender {
    let etag = body_etag(&body);
    CachedRender {
        body: Bytes::from(body),
        etag,
    }
}

pub(crate) fn body_etag(body: &str) -> HeaderValue {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(body.as_bytes());
    let formatted = format!("\"{}\"", hex::encode(&hasher.finalize()[..16]));
    // SHA-256 hex + quotes is pure ASCII, safe for HeaderValue.
    HeaderValue::try_from(formatted).expect("ascii etag")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_for(canonical_origin: &str) -> MarketingCfg {
        MarketingCfg {
            app_url: "https://app.uptimepage.dev".into(),
            canonical_origin: canonical_origin.into(),
            blog_enabled: true,
            mcp_url: None,
            checkout_open: false,
            trusted_proxies: Vec::new(),
        }
    }

    fn landing_html(canonical_origin: &str) -> String {
        // Renders straight past the per-process cache, so origin can vary.
        String::from_utf8(render_landing(&cfg_for(canonical_origin)).body.to_vec())
            .expect("utf8 body")
    }

    fn not_found_html(canonical_origin: &str) -> String {
        String::from_utf8(render_not_found(&cfg_for(canonical_origin)).body.to_vec())
            .expect("utf8 body")
    }

    fn pricing_html(checkout_open: bool) -> String {
        let cfg = MarketingCfg {
            checkout_open,
            ..cfg_for("https://uptimepage.dev")
        };
        String::from_utf8(render_pricing(&cfg).body.to_vec()).expect("utf8 body")
    }

    /// The paid cards may only send a visitor to checkout while the app
    /// serves one; otherwise the link would land on a 404 after sign-in.
    #[test]
    fn paid_plans_sell_only_while_checkout_is_open() {
        let open = pricing_html(true);
        assert!(open.contains("redirect_after=%2Fsettings%2Fbilling%3Fplan%3Dpro"));
        assert!(open.contains("data-cadence>"));
        assert!(!open.contains("mk-price--soon"));

        let closed = pricing_html(false);
        assert!(!closed.contains("settings%2Fbilling"));
        assert!(!closed.contains("data-cadence>"));
        assert!(!closed.contains("pricing.js"));
        assert_eq!(closed.matches("mk-price--soon").count(), 2);
        assert!(closed.contains("Tell%20me%20when%20Team%20launches"));
    }

    fn architecture_html(canonical_origin: &str) -> String {
        String::from_utf8(
            render_architecture(&cfg_for(canonical_origin))
                .body
                .to_vec(),
        )
        .expect("utf8 body")
    }

    /// The tracker is what defines `window.umami`, and every event helper on
    /// the site calls it optionally, so this one gate decides whether a
    /// deployment reports anything at all. Self-hosted must report nothing.
    #[test]
    fn analytics_renders_only_on_the_hosted_origin() {
        let hosted = landing_html("https://uptimepage.dev");
        assert!(hosted.contains("analytics.uptimepage.dev"));
        assert!(hosted.contains("data-website-id"));
        // A canonical carrying a path is the common case; the bare origin is
        // only the home page.
        assert!(architecture_html("https://uptimepage.dev").contains("data-website-id"));
        assert!(not_found_html("https://uptimepage.dev").contains("data-website-id"));

        for origin in [
            "https://status.acme.example",
            "http://localhost:8080",
            "https://uptimepage.dev.evil.example",
        ] {
            for body in [landing_html(origin), not_found_html(origin)] {
                assert!(
                    !body.contains("analytics.uptimepage.dev"),
                    "{origin} would report to our analytics"
                );
                assert!(
                    !body.contains("data-website-id"),
                    "{origin} would report to our analytics"
                );
            }
        }
    }

    /// The band arrives through an include resolved by name, so a rename would
    /// drop the apex's only URL capture without failing anything else.
    #[test]
    fn the_landing_carries_the_start_band() {
        let html = landing_html("https://uptimepage.dev");
        assert_eq!(html.matches(r#"action="/start""#).count(), 1);
        assert!(html.contains(r#"data-umami-event-position="band-url""#));
    }

    /// Both attributes are pure opt-ins in the tracker, and their absence is
    /// silent: the columns simply stay empty, which is how they went unnoticed.
    #[test]
    fn tracker_opts_into_web_vitals_and_the_variant_tag() {
        let hosted = landing_html("https://uptimepage.dev");
        assert!(hosted.contains(r#"data-performance="true""#));
        assert!(hosted.contains(&format!(
            r#"data-tag="{}""#,
            super::super::config::ANALYTICS_TAG
        )));
    }

    #[test]
    fn trailing_slash_redirects_to_the_bare_path() {
        let redirect = |s: &str| slash_redirect(&s.parse::<Uri>().expect("valid uri"));

        assert_eq!(redirect("/blog/").as_deref(), Some("/blog"));
        assert_eq!(
            redirect("/docs/self-hosting/").as_deref(),
            Some("/docs/self-hosting")
        );
        assert_eq!(
            redirect("/blog/?utm_source=x").as_deref(),
            Some("/blog?utm_source=x")
        );

        for already_fine in ["/", "/blog", "/blog?a=1"] {
            assert_eq!(
                redirect(already_fine),
                None,
                "{already_fine} needs no fixup"
            );
        }
        // `//host/` in a Location header is protocol-relative: the browser
        // would leave the site entirely.
        for off_site in ["//evil.example/", "///"] {
            assert_eq!(redirect(off_site), None, "{off_site} would leave the site");
        }
    }
}
