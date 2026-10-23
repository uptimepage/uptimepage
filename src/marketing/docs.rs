//! Product documentation on the marketing host: a `/docs` index plus one
//! page per Markdown source under `docs/`. Same slice-driven shape as
//! [`super::legal`] and [`super::landings`] — the [`DOCS`] table feeds the
//! route lookup, the render cache, the sidebar, and the sitemap. `build.rs`
//! generates it from `docs/index.toml`, so a new page is one manifest entry
//! plus one file.
//!
//! Sources are pulled in with `include_str!`, which registers a compiler
//! dependency per file — unlike the blog's `include_dir!`, an edited page
//! cannot survive into a warm rebuild.
//!
//! The renderer sanitises. Docs take third-party pull requests exactly
//! like the blog, so the trusted legal renderer must not be reused here;
//! the allowlist is only widened for what this module's own transforms
//! emit: heading ids, and the highlighter's token classes.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use askama::Template;
use askama_web::WebTemplate;
use axum::Router;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::http::HeaderValue;
use axum::response::Response;
use axum::routing::get;

use super::config::{BRAND, MarketingCfg};
use super::md::TocEntry;
use super::pages::{CachedRender, cached_render, not_found_page, serve_cached};
use super::seo::{
    JsonLd, OpenGraph, json_ld_breadcrumb, json_ld_breadcrumb_trail, json_ld_tech_article,
};
use crate::templates::filters;

/// A deploy can change the sidebar on every page at once, so an
/// already-visited page serves its old nav until this lapses.
const DOCS_CACHE_CONTROL: HeaderValue =
    HeaderValue::from_static("public, max-age=3600, stale-while-revalidate=86400");

pub const DOCS_INDEX_PATH: &str = "/docs";

/// Which readers a page is written for. Purely an orientation signal —
/// nothing here is gated, and self-hosters see the hosted pages just as
/// hosted users see the deployment ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Everyone,
    SelfHosting,
    Hosted,
}

impl Scope {
    pub fn badge(self) -> Option<&'static str> {
        match self {
            Scope::Everyone => None,
            Scope::SelfHosting => Some("self-hosting"),
            Scope::Hosted => Some("hosted"),
        }
    }
}

/// Sidebar grouping, rendered in declaration order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Section {
    Start,
    Guide,
    Reference,
    SelfHosting,
    Hosted,
}

/// Reading order, hosted before self-hosting: most readers are on the
/// hosted service, and a self-hoster looking for their section finds it
/// either way.
pub const SECTIONS: &[Section] = &[
    Section::Start,
    Section::Guide,
    Section::Reference,
    Section::Hosted,
    Section::SelfHosting,
];

impl Section {
    pub fn label(self) -> &'static str {
        match self {
            Section::Start => "start here",
            Section::Guide => "using uptimepage",
            Section::Reference => "reference",
            Section::SelfHosting => "self-hosting",
            Section::Hosted => "hosted service",
        }
    }

    pub fn blurb(self) -> &'static str {
        match self {
            Section::Start => "What the service is and how the pieces fit together.",
            Section::Guide => "Day-to-day work: monitors, incidents, status pages, alerts.",
            Section::Reference => "The API, Terraform, MCP, and the limits everything runs under.",
            Section::SelfHosting => "Running your own instance: deploy, configure, operate, debug.",
            Section::Hosted => "What differs when we run it for you at uptimepage.dev.",
        }
    }
}

pub struct DocPage {
    /// Path under `/docs`, mirroring the source layout under `docs/`.
    pub slug: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    pub blurb: &'static str,
    pub section: Section,
    pub scope: Scope,
    /// `datePublished`; never later than `lastmod`.
    pub created: &'static str,
    pub lastmod: &'static str,
    source: &'static str,
}

impl DocPage {
    pub fn path(&self) -> String {
        format!("{DOCS_INDEX_PATH}/{}", self.slug)
    }

    /// Source Markdown, pre-render — inlined verbatim into `llms-full.txt`.
    pub fn body_md(&self) -> &'static str {
        self.source
    }

    /// The source file's directory under `docs/`, so relative links in it
    /// resolve the way they do on disk.
    fn dir(&self) -> &'static str {
        self.slug.rsplit_once('/').map_or("", |(dir, _)| dir)
    }
}

/// Generated from `docs/index.toml`, in the order readers see within each
/// section.
pub const DOCS: &[DocPage] = include!(concat!(env!("OUT_DIR"), "/docs_manifest.rs"));

pub fn find(slug: &str) -> Option<&'static DocPage> {
    DOCS.iter().find(|d| d.slug == slug)
}

/// The index lists every page's title and blurb, so it changes whenever any
/// of them does. Dates are ISO, so lexical max is chronological.
pub fn index_lastmod() -> Option<&'static str> {
    DOCS.iter().map(|d| d.lastmod).max()
}

/// Sanitising render. The allowlist adds ids on h2–h4, plus whatever the
/// highlighter needs. The visible sidebar keeps the h2/h3 subset; an h4 id
/// exists only as a deep-link target.
fn render(markdown: &str, dir: &str) -> (String, Vec<TocEntry>) {
    let mut opts = pulldown_cmark::Options::empty();
    opts.insert(pulldown_cmark::Options::ENABLE_TABLES);
    opts.insert(pulldown_cmark::Options::ENABLE_STRIKETHROUGH);
    let parser = pulldown_cmark::Parser::new_ext(markdown, opts);
    let (events, toc) =
        super::md::anchor_headings(super::md::rewrite_doc_links(parser, dir).collect());
    let events = super::highlight::code_blocks(events.into_iter());
    let mut html = String::new();
    pulldown_cmark::html::push_html(&mut html, super::md::wrap_tables(events.into_iter()));
    let mut safe = ammonia::Builder::default();
    safe.link_rel(Some("noopener noreferrer"))
        .add_allowed_classes("div", &["mk-table-scroll"])
        .add_tag_attributes("div", &["tabindex"])
        .add_tag_attributes("h2", &["id"])
        .add_tag_attributes("h3", &["id"])
        .add_tag_attributes("h4", &["id"]);
    super::highlight::allow_markup(&mut safe);
    (safe.clean(&html).to_string(), toc)
}

/// A sidebar link. Resolved once so the nav is identical on every page
/// apart from which entry is current.
#[derive(Debug, Clone)]
pub struct NavItem {
    pub href: String,
    pub title: &'static str,
    pub slug: &'static str,
    pub badge: Option<&'static str>,
    pub blurb: &'static str,
}

#[derive(Debug, Clone)]
pub struct NavSection {
    pub label: &'static str,
    pub blurb: &'static str,
    pub items: Vec<NavItem>,
}

static NAV: OnceLock<Vec<NavSection>> = OnceLock::new();

fn nav() -> &'static [NavSection] {
    NAV.get_or_init(|| {
        SECTIONS
            .iter()
            .map(|section| NavSection {
                label: section.label(),
                blurb: section.blurb(),
                items: DOCS
                    .iter()
                    .filter(|d| d.section == *section)
                    .map(|d| NavItem {
                        href: d.path(),
                        title: d.title,
                        slug: d.slug,
                        badge: d.scope.badge(),
                        blurb: d.blurb,
                    })
                    .collect(),
            })
            .collect()
    })
}

#[derive(Template, WebTemplate)]
#[template(path = "marketing/docs_page.html")]
struct DocsPage {
    canonical_url: String,
    app_url: String,
    og: OpenGraph,
    article_ld: JsonLd,
    breadcrumb_ld: JsonLd,
    title: &'static str,
    badge: Option<&'static str>,
    lastmod: &'static str,
    body_html: String,
    toc: Vec<TocEntry>,
    sections: &'static [NavSection],
    current: &'static str,
    version: &'static str,
}

#[derive(Template, WebTemplate)]
#[template(path = "marketing/docs_index.html")]
struct DocsIndex {
    canonical_url: String,
    app_url: String,
    og: OpenGraph,
    breadcrumb_ld: JsonLd,
    sections: &'static [NavSection],
    current: &'static str,
    version: &'static str,
}

static RENDERED: OnceLock<HashMap<&'static str, CachedRender>> = OnceLock::new();
static SOURCES: OnceLock<HashMap<&'static str, CachedRender>> = OnceLock::new();
static INDEX_CACHED: OnceLock<CachedRender> = OnceLock::new();

/// Served verbatim under `Accept: text/markdown` — same bytes the HTML is
/// rendered from, so the two can never disagree.
fn sources() -> HashMap<&'static str, CachedRender> {
    DOCS.iter()
        .map(|doc| (doc.slug, cached_render(doc.source.to_string())))
        .collect()
}

fn render_all(cfg: &MarketingCfg) -> HashMap<&'static str, CachedRender> {
    DOCS.iter()
        .map(|doc| {
            let path = doc.path();
            let canonical_url = format!("{}{path}", cfg.canonical_origin);
            let mut og = OpenGraph::default_for(
                &format!("{} | {BRAND} docs", doc.title),
                &canonical_url,
                &cfg.canonical_origin,
            );
            og.description = doc.description.to_string();
            let (body_html, mut toc) = render(doc.source, doc.dir());
            // h4 carries an id so deep links resolve; the visible contents stay h2/h3.
            toc.retain(|t| t.level < 4);
            let page = DocsPage {
                article_ld: json_ld_tech_article(
                    &cfg.canonical_origin,
                    &path,
                    doc.title,
                    doc.description,
                    doc.created,
                    doc.lastmod,
                    &og.image,
                ),
                breadcrumb_ld: json_ld_breadcrumb_trail(
                    &cfg.canonical_origin,
                    &[("Docs", DOCS_INDEX_PATH), (doc.title, &path)],
                ),
                canonical_url,
                app_url: cfg.app_url.clone(),
                og,
                title: doc.title,
                badge: doc.scope.badge(),
                lastmod: doc.lastmod,
                body_html,
                toc,
                sections: nav(),
                current: doc.slug,
                version: env!("CARGO_PKG_VERSION"),
            };
            let body = page
                .render()
                .unwrap_or_else(|e| format!("<!-- docs render failed: {e} -->"));
            (doc.slug, cached_render(body))
        })
        .collect()
}

const INDEX_DESCRIPTION: &str = "Documentation for uptimepage: getting started, monitor types, incidents, status pages, \
     the REST API, Terraform, MCP, self-hosting and the hosted service.";

fn render_index(cfg: &MarketingCfg) -> CachedRender {
    let canonical_url = format!("{}{DOCS_INDEX_PATH}", cfg.canonical_origin);
    let mut og = OpenGraph::default_for(
        &format!("Documentation | {BRAND}"),
        &canonical_url,
        &cfg.canonical_origin,
    );
    og.description = INDEX_DESCRIPTION.to_string();
    let body = DocsIndex {
        breadcrumb_ld: json_ld_breadcrumb(&cfg.canonical_origin, "Docs", DOCS_INDEX_PATH),
        canonical_url,
        app_url: cfg.app_url.clone(),
        og,
        sections: nav(),
        current: "",
        version: env!("CARGO_PKG_VERSION"),
    }
    .render()
    .unwrap_or_else(|e| format!("<!-- docs index render failed: {e} -->"));
    cached_render(body)
}

pub(crate) fn warm(cfg: &MarketingCfg) {
    INDEX_CACHED.get_or_init(|| render_index(cfg));
    RENDERED.get_or_init(|| render_all(cfg));
    SOURCES.get_or_init(sources);
}

async fn index(State(cfg): State<Arc<MarketingCfg>>, headers: HeaderMap) -> Response {
    let cached = INDEX_CACHED.get_or_init(|| render_index(&cfg));
    serve_cached(&headers, cached, &DOCS_CACHE_CONTROL)
}

async fn page(
    State(cfg): State<Arc<MarketingCfg>>,
    Path(slug): Path<String>,
    headers: HeaderMap,
) -> Response {
    let cache = RENDERED.get_or_init(|| render_all(&cfg));
    let slug = slug.trim_end_matches('/');
    match cache.get(slug) {
        Some(cached) => {
            let source = SOURCES
                .get_or_init(sources)
                .get(slug)
                .expect("DOCS drives both");
            super::negotiate::serve(&headers, cached, source, &DOCS_CACHE_CONTROL)
        }
        None => not_found_page(&cfg),
    }
}

/// One wildcard route rather than a mount per entry: slugs are nested
/// (`hosted/regions`), and the lookup is the same table either way.
pub fn mount(router: Router<Arc<MarketingCfg>>) -> Router<Arc<MarketingCfg>> {
    router
        .route(DOCS_INDEX_PATH, get(index))
        .route("/docs/{*slug}", get(page))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `SECTIONS` drives the sidebar and the index, so a page in a section
    /// missing from it renders nowhere despite being routed.
    #[test]
    fn every_section_in_use_is_listed() {
        for doc in DOCS {
            assert!(
                SECTIONS.contains(&doc.section),
                "{}: section {:?} is not in SECTIONS, so the page has no nav entry",
                doc.slug,
                doc.section
            );
        }
    }

    #[test]
    fn the_index_description_fits_serp_limits() {
        let width = INDEX_DESCRIPTION.chars().count();
        assert!((150..=160).contains(&width), "{width} chars, want 150-160");
    }

    /// Replaces the link checker that retired with the mdBook build: every
    /// cross-page link must resolve to a published page, and every anchor
    /// to a heading id that page actually emits.
    #[test]
    fn internal_links_and_anchors_resolve() {
        let rendered: HashMap<&str, (String, Vec<TocEntry>)> = DOCS
            .iter()
            .map(|doc| (doc.slug, render(doc.source, doc.dir())))
            .collect();
        for doc in DOCS {
            let (html, _) = &rendered[doc.slug];
            for href in hrefs(html) {
                // A bare `#anchor` stays same-page, so it resolves against
                // the linking document rather than another entry.
                let (slug, anchor) = match href.strip_prefix('#') {
                    Some(anchor) => (doc.slug, Some(anchor)),
                    None => {
                        let Some(rest) = href.strip_prefix("/docs/") else {
                            continue;
                        };
                        match rest.split_once('#') {
                            Some((s, a)) => (s, Some(a)),
                            None => (rest, None),
                        }
                    }
                };
                assert!(
                    find(slug).is_some(),
                    "{}: links to /docs/{slug}, which is not a published page",
                    doc.slug
                );
                let Some(anchor) = anchor else { continue };
                let (_, toc) = &rendered[slug];
                assert!(
                    toc.iter().any(|t| t.id == anchor),
                    "{}: links to /docs/{slug}#{anchor}, which that page has no heading for",
                    doc.slug
                );
            }
        }
    }

    fn hrefs(html: &str) -> Vec<String> {
        html.match_indices("href=\"")
            .filter_map(|(i, m)| {
                let rest = &html[i + m.len()..];
                rest.find('"').map(|end| rest[..end].to_string())
            })
            .collect()
    }

    #[test]
    fn headings_keep_their_ids_through_sanitisation() {
        let (html, toc) = render("## Retry budget\n\ntext\n", "");
        assert_eq!(toc.len(), 1);
        assert_eq!(toc[0].id, "retry-budget");
        assert!(html.contains("id=\"retry-budget\""), "got: {html}");
    }

    #[test]
    fn renderer_strips_script_and_handlers() {
        let html = render(
            "<script>alert(1)</script>\n\n<img src=x onerror=alert(1)>",
            "",
        )
        .0;
        assert!(!html.contains("<script"), "got: {html}");
        assert!(!html.contains("onerror"), "got: {html}");
    }

    #[test]
    fn relative_doc_links_become_site_paths() {
        let html = render("[api](api.md) and [cfg](configuration.md#logging)", "").0;
        assert!(html.contains("href=\"/docs/api\""), "got: {html}");
        assert!(
            html.contains("href=\"/docs/configuration#logging\""),
            "got: {html}"
        );
    }

    #[test]
    fn parent_relative_links_resolve_from_the_source_directory() {
        let html = render("[api](../api.md)", "hosted").0;
        assert!(html.contains("href=\"/docs/api\""), "got: {html}");
    }
}
