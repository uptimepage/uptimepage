//! Build-time compiled changelog. One Markdown file per shipped change under
//! `src/marketing/content/changelog/`, rendered once at boot the same way the
//! blog is: sanitised HTML, a Markdown twin for `Accept: text/markdown`, and an
//! Atom feed, all held in `OnceLock` caches.
//!
//! An entry is dated prose about what changed, not reference material, so it
//! carries `Article` rather than `TechArticle` and never replaces the docs it
//! links to.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use askama::Template;
use askama_web::WebTemplate;
use axum::Router;
use axum::extract::{Path, State};
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use bytes::Bytes;
use include_dir::{Dir, include_dir};
use serde::Deserialize;

use super::blog::{render, split_front_matter};
use super::config::{BRAND, MarketingCfg};
use super::pages::{CachedRender, cached_render, not_found_page, serve_cached};
use super::seo::{JsonLd, OpenGraph, json_ld_article, json_ld_breadcrumb, xml_escape};
use crate::web::filters;

pub const INDEX_PATH: &str = "/changelog";
pub const FEED_PATH: &str = "/changelog.xml";

const PAGE_CACHE_CONTROL: HeaderValue =
    HeaderValue::from_static("public, max-age=600, stale-while-revalidate=86400");
const FEED_CACHE_CONTROL: HeaderValue = HeaderValue::from_static("public, max-age=3600");
const ATOM: HeaderValue = HeaderValue::from_static("application/atom+xml; charset=utf-8");

static ENTRY_DIR: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/src/marketing/content/changelog");

#[derive(Debug, Clone)]
pub struct Entry {
    pub slug: String,
    pub title: String,
    pub date: String,
    /// One sentence for the index, the feed and the meta description.
    pub summary: String,
    pub body_html: String,
    /// Source Markdown, inlined verbatim into `llms-full.txt`.
    pub body_md: String,
}

impl Entry {
    pub fn path(&self) -> String {
        format!("{INDEX_PATH}/{}", self.slug)
    }
}

#[derive(Debug, Deserialize)]
struct FrontMatter {
    title: String,
    date: String,
    summary: String,
    slug: Option<String>,
}

static ENTRIES: OnceLock<Vec<Entry>> = OnceLock::new();
static INDEX_CACHED: OnceLock<CachedRender> = OnceLock::new();
static RENDERED: OnceLock<HashMap<String, CachedRender>> = OnceLock::new();
static SOURCES: OnceLock<HashMap<String, CachedRender>> = OnceLock::new();
static FEED_CACHED: OnceLock<Bytes> = OnceLock::new();

/// Newest first; a tie on the day falls back to the title so the order is
/// the same on every boot.
pub fn entries() -> &'static [Entry] {
    ENTRIES.get_or_init(load_entries).as_slice()
}

/// The newest entry's date, for the index's `lastmod` and the feed's `updated`.
pub fn latest_date() -> Option<&'static str> {
    entries().first().map(|e| e.date.as_str())
}

pub(crate) fn warm(cfg: &MarketingCfg) {
    entries();
    INDEX_CACHED.get_or_init(|| render_index(cfg));
    RENDERED.get_or_init(|| render_all(cfg));
    SOURCES.get_or_init(sources);
    FEED_CACHED.get_or_init(|| Bytes::from(build_feed(cfg)));
}

fn load_entries() -> Vec<Entry> {
    let mut out: Vec<Entry> = ENTRY_DIR
        .files()
        .filter(|f| f.path().extension().and_then(|e| e.to_str()) == Some("md"))
        .filter_map(|f| {
            let stem = f.path().file_stem()?.to_str()?;
            match parse_entry(f.contents_utf8()?, stem) {
                Ok(entry) => Some(entry),
                Err(e) => {
                    tracing::error!(file = %f.path().display(), error = %e, "changelog entry parse failed");
                    None
                }
            }
        })
        .collect();
    out.sort_by(|a, b| b.date.cmp(&a.date).then_with(|| a.title.cmp(&b.title)));
    out
}

fn parse_entry(raw: &str, stem: &str) -> anyhow::Result<Entry> {
    let (front, body) = split_front_matter(raw)
        .ok_or_else(|| anyhow::anyhow!("missing TOML front-matter block"))?;
    let fm: FrontMatter = toml::from_str(front)?;
    anyhow::ensure!(
        is_calendar_date(&fm.date),
        "date must be a real YYYY-MM-DD day, got {:?}",
        fm.date
    );
    Ok(Entry {
        slug: fm.slug.unwrap_or_else(|| stem.to_string()),
        title: fm.title,
        date: fm.date,
        summary: fm.summary,
        body_html: render(body),
        body_md: body.trim().to_string(),
    })
}

/// Shape and calendar both: a typo like `2026-09-31` would otherwise reach
/// the sitemap, the feed and the JSON-LD, where validators reject it.
fn is_calendar_date(date: &str) -> bool {
    date.len() == 10 && chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d").is_ok()
}

#[derive(Template, WebTemplate)]
#[template(path = "marketing/changelog_index.html")]
struct IndexPage {
    canonical_url: String,
    app_url: String,
    og: OpenGraph,
    breadcrumb_ld: JsonLd,
    entries: &'static [Entry],
    version: &'static str,
}

#[derive(Template, WebTemplate)]
#[template(path = "marketing/changelog_entry.html")]
struct EntryPage {
    canonical_url: String,
    app_url: String,
    og: OpenGraph,
    article_ld: JsonLd,
    breadcrumb_ld: JsonLd,
    entry: &'static Entry,
    version: &'static str,
}

fn render_index(cfg: &MarketingCfg) -> CachedRender {
    let canonical_url = format!("{}{INDEX_PATH}", cfg.canonical_origin);
    let mut og = OpenGraph::default_for(
        &format!("{BRAND} changelog"),
        &canonical_url,
        &cfg.canonical_origin,
    );
    og.description = INDEX_DESCRIPTION.to_string();
    let body = IndexPage {
        canonical_url,
        app_url: cfg.app_url.clone(),
        og,
        breadcrumb_ld: json_ld_breadcrumb(&cfg.canonical_origin, "Changelog", INDEX_PATH),
        entries: entries(),
        version: env!("CARGO_PKG_VERSION"),
    }
    .render()
    .unwrap_or_else(|e| format!("<!-- changelog index render failed: {e} -->"));
    cached_render(body)
}

pub const INDEX_DESCRIPTION: &str = "What shipped in Uptimepage and when: new checks, channels, status page controls, MCP tools and fixes, one dated entry each.";

fn render_entry(cfg: &MarketingCfg, entry: &'static Entry) -> CachedRender {
    let path = entry.path();
    let canonical_url = format!("{}{path}", cfg.canonical_origin);
    let mut og = OpenGraph::default_for(&entry.title, &canonical_url, &cfg.canonical_origin);
    og.description = entry.summary.clone();
    og.og_type = "article".to_string();
    let body = EntryPage {
        canonical_url,
        app_url: cfg.app_url.clone(),
        og,
        article_ld: json_ld_article(
            &cfg.canonical_origin,
            &path,
            &entry.title,
            &entry.summary,
            &entry.date,
        ),
        breadcrumb_ld: super::seo::json_ld_breadcrumb_trail(
            &cfg.canonical_origin,
            &[("Changelog", INDEX_PATH), (&entry.title, &path)],
        ),
        entry,
        version: env!("CARGO_PKG_VERSION"),
    }
    .render()
    .unwrap_or_else(|e| format!("<!-- changelog entry render failed: {e} -->"));
    cached_render(body)
}

fn render_all(cfg: &MarketingCfg) -> HashMap<String, CachedRender> {
    entries()
        .iter()
        .map(|e| (e.slug.clone(), render_entry(cfg, e)))
        .collect()
}

fn sources() -> HashMap<String, CachedRender> {
    entries()
        .iter()
        .map(|e| {
            let doc = format!("# {}\n\n_{}_\n\n{}", e.title, e.date, e.body_md);
            (e.slug.clone(), cached_render(doc))
        })
        .collect()
}

/// Atom over RSS: dates are RFC 3339, which a bare `YYYY-MM-DD` extends
/// without a calendar, and `content type="html"` carries the sanitised body
/// as-is.
fn build_feed(cfg: &MarketingCfg) -> String {
    let origin = &cfg.canonical_origin;
    let updated = latest_date().map(atom_time).unwrap_or_default();
    let mut s = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    // Entry bodies keep their root-relative hrefs; `xml:base` is what a
    // reader resolves them against.
    s.push_str(&format!(
        "<feed xmlns=\"http://www.w3.org/2005/Atom\" xml:base=\"{origin}/\">\n"
    ));
    s.push_str(&format!("  <id>{origin}{INDEX_PATH}</id>\n"));
    s.push_str(&format!("  <title>{BRAND} changelog</title>\n"));
    s.push_str(&format!(
        "  <subtitle>{}</subtitle>\n",
        xml_escape(INDEX_DESCRIPTION)
    ));
    s.push_str(&format!(
        "  <link rel=\"self\" type=\"application/atom+xml\" href=\"{origin}{FEED_PATH}\"/>\n"
    ));
    s.push_str(&format!(
        "  <link rel=\"alternate\" type=\"text/html\" href=\"{origin}{INDEX_PATH}\"/>\n"
    ));
    s.push_str(&format!("  <updated>{updated}</updated>\n"));
    s.push_str(&format!("  <author><name>{BRAND}</name></author>\n"));
    for e in entries() {
        let url = format!("{origin}{}", e.path());
        let when = atom_time(&e.date);
        s.push_str("  <entry>\n");
        s.push_str(&format!("    <id>{url}</id>\n"));
        s.push_str(&format!("    <title>{}</title>\n", xml_escape(&e.title)));
        s.push_str(&format!(
            "    <link rel=\"alternate\" type=\"text/html\" href=\"{url}\"/>\n"
        ));
        s.push_str(&format!("    <published>{when}</published>\n"));
        s.push_str(&format!("    <updated>{when}</updated>\n"));
        s.push_str(&format!(
            "    <summary>{}</summary>\n",
            xml_escape(&e.summary)
        ));
        s.push_str(&format!(
            "    <content type=\"html\">{}</content>\n",
            xml_escape(&e.body_html)
        ));
        s.push_str("  </entry>\n");
    }
    s.push_str("</feed>\n");
    s
}

fn atom_time(date: &str) -> String {
    format!("{date}T00:00:00Z")
}

async fn index(State(cfg): State<Arc<MarketingCfg>>, headers: HeaderMap) -> Response {
    let cached = INDEX_CACHED.get_or_init(|| render_index(&cfg));
    serve_cached(&headers, cached, &PAGE_CACHE_CONTROL)
}

async fn entry(
    State(cfg): State<Arc<MarketingCfg>>,
    Path(slug): Path<String>,
    headers: HeaderMap,
) -> Response {
    let cache = RENDERED.get_or_init(|| render_all(&cfg));
    match cache.get(&slug) {
        Some(cached) => {
            let source = SOURCES
                .get_or_init(sources)
                .get(&slug)
                .expect("same entry list");
            super::negotiate::serve(&headers, cached, source, &PAGE_CACHE_CONTROL)
        }
        None => not_found_page(&cfg),
    }
}

async fn feed(State(cfg): State<Arc<MarketingCfg>>) -> Response {
    let body = FEED_CACHED.get_or_init(|| Bytes::from(build_feed(&cfg)));
    (
        StatusCode::OK,
        [(CONTENT_TYPE, ATOM), (CACHE_CONTROL, FEED_CACHE_CONTROL)],
        body.clone(),
    )
        .into_response()
}

pub fn mount(router: Router<Arc<MarketingCfg>>) -> Router<Arc<MarketingCfg>> {
    router
        .route(INDEX_PATH, get(index))
        .route("/changelog/{slug}", get(entry))
        .route(FEED_PATH, get(feed))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A file that fails to parse is logged and dropped, which every other
    /// assertion here would pass vacuously; the count is what proves the
    /// directory made it into the binary.
    #[test]
    fn every_source_file_becomes_an_entry() {
        let files = ENTRY_DIR
            .files()
            .filter(|f| f.path().extension().and_then(|e| e.to_str()) == Some("md"))
            .count();
        assert!(files > 0, "no changelog sources compiled in");
        assert_eq!(entries().len(), files, "an entry failed to parse");
    }

    #[test]
    fn entries_are_newest_first_with_a_stable_tiebreak() {
        let dates: Vec<&str> = entries().iter().map(|e| e.date.as_str()).collect();
        let mut sorted = dates.clone();
        sorted.sort_by(|a, b| b.cmp(a));
        assert_eq!(dates, sorted);
        for pair in entries().windows(2) {
            if pair[0].date == pair[1].date {
                assert!(
                    pair[0].title < pair[1].title,
                    "{:?} before {:?}",
                    pair[0].title,
                    pair[1].title
                );
            }
        }
    }

    #[test]
    fn entries_fit_serp_limits() {
        for e in entries() {
            assert!(
                e.title.len() <= 65,
                "{}: title is {} bytes",
                e.slug,
                e.title.len()
            );
            assert!(
                (40..=160).contains(&e.summary.len()),
                "{}: summary is {} bytes",
                e.slug,
                e.summary.len()
            );
        }
    }

    #[test]
    fn slugs_are_unique_url_safe_and_dated() {
        let mut seen = std::collections::HashSet::new();
        for e in entries() {
            assert!(seen.insert(&e.slug), "duplicate slug {}", e.slug);
            assert!(
                e.slug
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'),
                "{}: slug must be lowercase kebab",
                e.slug
            );
            assert!(is_calendar_date(&e.date), "{}: {}", e.slug, e.date);
        }
    }

    #[test]
    fn rejects_a_date_that_is_not_a_calendar_day() {
        for bad in [
            "2026-09-17T10:00:00Z",
            "2026-09-31",
            "2026-13-01",
            "26-09-17",
        ] {
            let raw = format!("+++\ntitle = \"t\"\ndate = \"{bad}\"\nsummary = \"s\"\n+++\nbody");
            assert!(parse_entry(&raw, "x").is_err(), "{bad} accepted");
        }
    }

    #[test]
    fn feed_is_atom_with_one_entry_per_change() {
        let cfg = MarketingCfg {
            app_url: "https://app.x.test".into(),
            canonical_origin: "https://x.test".into(),
            blog_enabled: false,
            mcp_url: None,
            checkout_open: false,
            trusted_proxies: Vec::new(),
        };
        let feed = build_feed(&cfg);
        assert!(feed.starts_with(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<feed xmlns=\"http://www.w3.org/2005/Atom\" xml:base=\"https://x.test/\">"
        ));
        assert_eq!(feed.matches("<entry>").count(), entries().len());
        assert!(feed.contains("<link rel=\"self\" type=\"application/atom+xml\" href=\"https://x.test/changelog.xml\"/>"));
        for e in entries() {
            assert!(feed.contains(&format!("<id>https://x.test/changelog/{}</id>", e.slug)));
            assert!(feed.contains(&format!("<published>{}T00:00:00Z</published>", e.date)));
        }
        assert!(
            !feed.contains("<content type=\"html\"><p>"),
            "body must be escaped, not embedded"
        );
    }
}
