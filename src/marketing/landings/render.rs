use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use askama::Template;
use askama_web::WebTemplate;
use axum::Router;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::HeaderValue;
use axum::response::{Redirect, Response};
use axum::routing::get;

use crate::marketing::config::{BRAND, MarketingCfg};
use crate::marketing::pages::{CachedRender, cached_render, serve_cached};
use crate::marketing::seo::{
    AUTHOR_PAGE, JsonLd, OpenGraph, json_ld_breadcrumb, json_ld_breadcrumb_trail, json_ld_faqpage,
    json_ld_person, json_ld_webpage,
};
use crate::web::filters;

use super::catalog::LANDINGS;
use super::faqs::page_faqs;
use super::matrices::page_matrix;
use super::model::{
    CodeSample, ConfigPane, Feature, Figure, Landing, Matrix, MockRow, PickCard, ResourceLink,
    Section, is_comparison,
};
use super::sections::{MOCK_ROWS, page_callout, page_config, page_figures, page_fit, page_picks};

const LANDING_CACHE_CONTROL: HeaderValue =
    HeaderValue::from_static("public, max-age=300, stale-while-revalidate=86400");

#[derive(Template, WebTemplate)]
#[template(path = "marketing/landing_page.html")]
struct LandingDoc {
    title: String,
    eyebrow: &'static str,
    h1: &'static str,
    lede: &'static str,
    features: &'static [Feature],
    sections: &'static [Section],
    code: Option<&'static CodeSample>,
    matrix: Option<&'static Matrix>,
    /// Head-to-head layout: the two-column hero, the pick cards, the
    /// callout and the closing pitch beside the mock.
    comparison: bool,
    picks: &'static [PickCard],
    config: &'static [ConfigPane],
    callout: Option<&'static Section>,
    fit: Option<&'static str>,
    mock_rows: &'static [MockRow],
    figures: &'static [Figure],
    resources: Vec<ResourceLink>,
    cta: &'static str,
    canonical_url: String,
    og: OpenGraph,
    breadcrumb_json_ld: JsonLd,
    webpage_json_ld: JsonLd,
    faq_json_ld: Option<JsonLd>,
    person_json_ld: Option<JsonLd>,
    faqs: &'static [(&'static str, &'static str)],
    app_url: String,
    version: &'static str,
}

static RENDERED: OnceLock<HashMap<&'static str, CachedRender>> = OnceLock::new();

/// The landing one level up (`/mcp-server` for `/mcp-server/cursor`), when
/// that is itself a landing.
pub(super) fn parent(l: &Landing) -> Option<&'static Landing> {
    let (parent_path, _) = l.path.rsplit_once('/')?;
    LANDINGS.iter().find(|p| p.path == parent_path)
}

/// A landing's own links, then its family: a hub gets every child, a child
/// gets the hub and every sibling. A new child page is linked from the whole
/// family without editing each one.
fn resources_with_family(l: &Landing) -> Vec<ResourceLink> {
    let mut out: Vec<ResourceLink> = l
        .resources
        .iter()
        .map(|r| ResourceLink {
            label: r.label,
            href: r.href,
        })
        .collect();
    let hub = parent(l).unwrap_or(l);
    if hub.path != l.path {
        out.push(ResourceLink {
            label: hub.title,
            href: hub.path,
        });
    }
    let prefix = format!("{}/", hub.path);
    out.extend(
        LANDINGS
            .iter()
            .filter(|s| s.path.starts_with(&prefix) && s.path != l.path)
            .map(|s| ResourceLink {
                label: s.title,
                href: s.path,
            }),
    );
    out
}

fn breadcrumb(origin: &str, l: &Landing) -> JsonLd {
    match parent(l) {
        Some(hub) => json_ld_breadcrumb_trail(origin, &[(hub.h1, hub.path), (l.h1, l.path)]),
        None => json_ld_breadcrumb(origin, l.h1, l.path),
    }
}

pub(super) fn render_all(cfg: &MarketingCfg) -> HashMap<&'static str, CachedRender> {
    LANDINGS
        .iter()
        .map(|l| {
            let canonical_url = format!("{}{}", cfg.canonical_origin, l.path);
            let title = format!("{} | {BRAND}", l.title);
            let mut og = OpenGraph::default_for(&title, &canonical_url, &cfg.canonical_origin);
            og.description = l.meta_description.to_string();
            let faqs = page_faqs(l.path);
            let doc = LandingDoc {
                title,
                eyebrow: l.eyebrow,
                h1: l.h1,
                lede: l.lede,
                features: l.features,
                sections: l.sections,
                code: l.code.as_ref(),
                matrix: page_matrix(l.path),
                comparison: is_comparison(l.path),
                picks: page_picks(l.path),
                config: page_config(l.path),
                callout: page_callout(l.path),
                fit: page_fit(l.path),
                mock_rows: MOCK_ROWS,
                figures: page_figures(l.path),
                resources: resources_with_family(l),
                cta: l.cta,
                canonical_url,
                og,
                breadcrumb_json_ld: breadcrumb(&cfg.canonical_origin, l),
                webpage_json_ld: json_ld_webpage(
                    &cfg.canonical_origin,
                    l.path,
                    l.h1,
                    l.created,
                    l.lastmod,
                    // /about describes the operator, not the product.
                    !l.path.starts_with("/compare/") && l.path != AUTHOR_PAGE,
                ),
                faq_json_ld: (!faqs.is_empty()).then(|| json_ld_faqpage(faqs)),
                person_json_ld: (l.path == AUTHOR_PAGE)
                    .then(|| json_ld_person(&cfg.canonical_origin)),
                faqs,
                app_url: cfg.app_url.clone(),
                version: env!("CARGO_PKG_VERSION"),
            };
            let body = doc
                .render()
                .unwrap_or_else(|e| format!("<!-- landing render failed: {e} -->"));
            (l.path, cached_render(body))
        })
        .collect()
}

/// Warm the per-page render cache at boot so the first hit doesn't pay
/// askama + SHA-256.
pub(crate) fn warm(cfg: &MarketingCfg) {
    RENDERED.get_or_init(|| render_all(cfg));
}

async fn serve(
    State(cfg): State<Arc<MarketingCfg>>,
    headers: HeaderMap,
    landing: &'static Landing,
) -> Response {
    let map = RENDERED.get_or_init(|| render_all(&cfg));
    let cached = map.get(landing.path).expect("LANDINGS drives render");
    serve_cached(&headers, cached, &LANDING_CACHE_CONTROL)
}

/// Mount every entry in [`LANDINGS`]. One source of truth — no per-page
/// handler, no `match` arm.
pub fn mount(router: Router<Arc<MarketingCfg>>) -> Router<Arc<MarketingCfg>> {
    let mut r = router;
    for landing in LANDINGS {
        r = r.route(
            landing.path,
            get(move |state, headers| serve(state, headers, landing)),
        );
    }
    for (from, to) in ALIASES {
        r = r.route(from, get(move || async move { Redirect::permanent(to) }));
    }
    r
}

/// Paths that earn traffic but should not be pages: retired URLs and the
/// spellings visitors guess. One source, so a test can prove every target is
/// still a real landing or a published post.
pub(super) const ALIASES: &[(&str, &str)] = &[
    // Old name for Better Stack; searchers still use it.
    ("/vs/better-uptime", "/vs/better-stack"),
    // /automation split the same Terraform intent as the page below and the same
    // MCP intent as /mcp-server, so it competed with both.
    ("/automation", "/terraform-uptime-monitoring"),
    // The roundup outranks the head-to-head page on these, so send them there.
    ("/pingdom-alternatives", "/blog/pingdom-alternatives"),
    ("/vs/pingdom-alternatives", "/blog/pingdom-alternatives"),
    // Bare head term readers and inbound links guess at; the roundup owns it.
    ("/statuspage-alternatives", "/blog/statuspage-alternatives"),
    (
        "/vs/statuspage-alternatives",
        "/blog/statuspage-alternatives",
    ),
    // Same pitch as the open-source page, which draws far more search demand.
    ("/self-hosted-status-page", "/open-source-status-page"),
    // Searchers name these pairs in either order; one page answers both.
    (
        "/compare/gatus-vs-uptime-kuma",
        "/compare/uptime-kuma-vs-gatus",
    ),
];
