//! Build-time compiled blog. Posts under `src/marketing/content/blog/`
//! are read at compile time via `include_dir!`, parsed and rendered to
//! sanitised HTML once at startup, and held in a `OnceLock<Vec<Post>>`
//! along with their fully-rendered page bodies + ETags.
//!
//! The Markdown renderer is vendored locally on purpose — blog content
//! arrives as third-party PR input, so every byte goes through
//! `ammonia::clean` before reaching a template. The legal-page renderer
//! is deliberately unsanitised (first-party tables) and must not be
//! reused here.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use askama::Template;
use askama_web::WebTemplate;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::http::HeaderValue;
use axum::response::Response;
use include_dir::{Dir, include_dir};
use serde::Deserialize;

use super::config::{AUTHOR, Author, BRAND, MarketingCfg};
use super::pages::{CachedRender, cached_render, not_found_page, serve_cached};
use super::seo::{
    JsonLd, OpenGraph, json_ld_blog, json_ld_blog_posting, json_ld_breadcrumb, json_ld_faqpage,
    json_ld_item_list,
};
use crate::web::filters;

const RELATED_LIMIT: usize = 3;
const POST_CACHE_CONTROL: HeaderValue =
    HeaderValue::from_static("public, max-age=600, stale-while-revalidate=86400");
const INDEX_CACHE_CONTROL: HeaderValue =
    HeaderValue::from_static("public, max-age=300, stale-while-revalidate=86400");

static BLOG_DIR: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/src/marketing/content/blog");

#[derive(Debug, Clone)]
pub struct Post {
    pub slug: String,
    pub title: String,
    /// Optional document title when it should differ from the visible post title.
    pub meta_title: Option<String>,
    pub date: String,
    pub updated: Option<String>,
    pub excerpt: String,
    pub tags: Vec<String>,
    pub draft: bool,
    /// Ranked item names for list-format posts; emits `ItemList` JSON-LD.
    pub list_items: Vec<String>,
    /// Question/answer pairs mirroring the post's visible FAQ; emits
    /// `FAQPage` JSON-LD.
    pub faqs: Vec<(String, String)>,
    /// Origin-relative path to a post-specific social card; the shared
    /// site card is used when absent.
    pub og_image: Option<String>,
    /// Unset on engineering posts: that audience is not choosing a monitor.
    pub cta_label: Option<String>,
    pub body_html: String,
    /// Byte offset in `body_html` where the mid-article start band belongs.
    pub band_at: Option<usize>,
    pub embed_scripts: Vec<&'static str>,
    /// Source markdown, pre-render — inlined verbatim into `llms-full.txt`.
    pub body_md: String,
    /// Local images the post renders, for the image sitemap.
    pub images: Vec<PostImage>,
}

#[derive(Debug, Clone)]
pub struct PostImage {
    /// Unfingerprinted: a path the page doesn't render indexes as a second image.
    pub path: String,
    pub alt: String,
}

#[derive(Debug, Deserialize)]
struct FrontMatter {
    title: String,
    meta_title: Option<String>,
    date: String,
    updated: Option<String>,
    slug: Option<String>,
    #[serde(default)]
    excerpt: String,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    list_items: Vec<String>,
    #[serde(default)]
    faqs: Vec<FaqEntry>,
    og_image: Option<String>,
    cta_label: Option<String>,
}

#[derive(Debug, Deserialize)]
struct FaqEntry {
    q: String,
    a: String,
}

static POSTS: OnceLock<Vec<Post>> = OnceLock::new();
static RENDERED_INDEX: OnceLock<HashMap<String, CachedRender>> = OnceLock::new();
static SOURCES: OnceLock<HashMap<String, CachedRender>> = OnceLock::new();
static INDEX_CACHED: OnceLock<CachedRender> = OnceLock::new();

/// Parse + render every published post once. Idempotent; the renders
/// land in static caches the request handlers read directly.
pub fn init() -> &'static [Post] {
    POSTS.get_or_init(load_posts).as_slice()
}

pub fn all() -> &'static [Post] {
    init()
}

pub fn list_published() -> Vec<&'static Post> {
    let drafts_visible = cfg!(debug_assertions);
    init()
        .iter()
        .filter(|p| drafts_visible || !p.draft)
        .collect()
}

/// Warm post-load + index + per-post render caches at boot. Cheap
/// relative to the cold-first-request penalty (parse + sanitise +
/// askama + SHA-256 on every published post).
pub(crate) fn warm(cfg: &MarketingCfg) {
    init();
    INDEX_CACHED.get_or_init(|| render_index(cfg));
    RENDERED_INDEX.get_or_init(|| build_post_index(cfg));
    SOURCES.get_or_init(build_sources);
}

/// The post body under `Accept: text/markdown`, with the front-matter
/// title and date restored as Markdown so the served document stands on
/// its own.
fn build_sources() -> HashMap<String, CachedRender> {
    list_published()
        .into_iter()
        .map(|p| {
            let doc = format!("# {}\n\n_{}_\n\n{}", p.title, p.date, p.body_md);
            (p.slug.clone(), cached_render(doc))
        })
        .collect()
}

fn build_post_index(cfg: &MarketingCfg) -> HashMap<String, CachedRender> {
    list_published()
        .into_iter()
        .map(|p| (p.slug.clone(), render_post(cfg, p)))
        .collect()
}

fn load_posts() -> Vec<Post> {
    let mut out: Vec<Post> = Vec::new();
    for f in BLOG_DIR.files() {
        let path = f.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let raw = match f.contents_utf8() {
            Some(s) => s,
            None => continue,
        };
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("post")
            .to_string();
        match parse_post(raw, &stem) {
            Ok(post) => out.push(post),
            Err(e) => {
                tracing::error!(file = %path.display(), error = %e, "blog post parse failed");
            }
        }
    }
    out.sort_by(|a, b| b.date.cmp(&a.date));
    out
}

fn parse_post(raw: &str, stem: &str) -> anyhow::Result<Post> {
    let (front, body) = split_front_matter(raw)
        .ok_or_else(|| anyhow::anyhow!("missing TOML front-matter block"))?;
    let fm: FrontMatter = toml::from_str(front)?;
    if let Some(img) = &fm.og_image {
        anyhow::ensure!(
            img.starts_with("/static/"),
            "og_image must be a /static/-rooted path, got {img:?}"
        );
    }
    let (body_html, band_at) = render_with_band(body);
    Ok(Post {
        slug: fm.slug.unwrap_or_else(|| stem.to_string()),
        title: fm.title,
        meta_title: fm.meta_title,
        date: fm.date,
        updated: fm.updated,
        excerpt: fm.excerpt,
        tags: fm.tags,
        draft: fm.draft,
        list_items: fm.list_items,
        faqs: fm.faqs.into_iter().map(|f| (f.q, f.a)).collect(),
        og_image: fm.og_image,
        cta_label: fm.cta_label,
        embed_scripts: embed_scripts(&body_html),
        body_html,
        band_at,
        body_md: body.trim().to_string(),
        images: collect_images(body),
    })
}

fn split_front_matter(raw: &str) -> Option<(&str, &str)> {
    let stripped = raw.strip_prefix("+++\n")?;
    let end = stripped.find("\n+++\n")?;
    let front = &stripped[..end];
    let body = &stripped[end + "\n+++\n".len()..];
    Some((front, body))
}

/// Interactive figures a post can embed: the mount class it writes in Markdown,
/// and the script that fills it. One table so the sanitiser allowlist and the
/// script the page loads cannot drift apart.
const EMBEDS: &[(&str, &str)] = &[
    ("mk-embed-quorum", "js/marketing/quorum.js"),
    ("mk-embed-gate", "js/marketing/gate.js"),
    ("mk-embed-silence", "js/marketing/silence.js"),
    ("mk-embed-flow-break", "js/marketing/flow_break.js"),
    ("mk-embed-ci-vs-prod", "js/marketing/ci_vs_prod.js"),
    ("mk-embed-stagger", "js/marketing/stagger.js"),
    ("mk-embed-supersede", "js/marketing/supersede.js"),
    ("mk-embed-measured-week", "js/marketing/measured_week.js"),
    ("mk-embed-blind", "js/marketing/blind.js"),
    ("mk-embed-grace", "js/marketing/grace.js"),
];

/// A macro, not a const: `concat!` needs a literal to build [`BAND_MARK`]
/// from the same source.
macro_rules! band_class {
    () => {
        "mk-band"
    };
}
const BAND_CLASS: &str = band_class!();

/// Injected into the Markdown to carry the band's position through rendering,
/// then cut back out. A serialised tag survives ammonia; a comment would not.
const BAND_MARK: &str = concat!("<div class=\"", band_class!(), "\"></div>");

/// Sanitising Markdown render. CommonMark + tables → HTML → ammonia
/// allowlist. Third-party PR content never reaches a browser as raw
/// HTML; every `<script>`, `onerror=`, and `javascript:` href is
/// dropped before the bytes leave this function.
pub fn render(markdown: &str) -> String {
    let mut html = String::new();
    let events = super::highlight::code_blocks(parser(markdown));
    pulldown_cmark::html::push_html(&mut html, super::md::wrap_tables(events.into_iter()));
    // Declared before the builder so it outlives the borrow taken below.
    let mounts: Vec<&str> = EMBEDS.iter().map(|(mount, _)| *mount).collect();
    let mut safe = ammonia::Builder::default();
    safe.link_rel(Some("noopener noreferrer"))
        .add_tags(["details", "summary"])
        .add_allowed_classes("div", &["mk-table-scroll", "mk-faq__body", BAND_CLASS])
        .add_allowed_classes("details", &["mk-faq"])
        .add_tag_attributes("div", &["tabindex"]);
    // Token spans and the `language-*` class the wrap rules key off. A class
    // cannot execute, so this widens nothing dangerous.
    super::highlight::allow_markup(&mut safe);
    safe.add_allowed_classes("div", &mounts);
    safe.clean(&html).to_string()
}

const VOID_TAGS: [&str; 14] = [
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source",
    "track", "wbr",
];

/// Exact on sanitised output; a heuristic on raw source, where a
/// commented-out tag still counts.
fn tag_balance(html: &str) -> isize {
    let mut balance = 0isize;
    let mut rest = html;
    while let Some(off) = rest.find('<') {
        rest = &rest[off + 1..];
        if let Some(after) = rest.strip_prefix('/') {
            let closed: String = after
                .chars()
                .take_while(char::is_ascii_alphanumeric)
                .flat_map(char::to_lowercase)
                .collect();
            // A void tag's opener is skipped, so honouring `</br>` here would
            // drive the balance negative against an opener that never counted.
            if !closed.is_empty() && !VOID_TAGS.contains(&closed.as_str()) {
                balance -= 1;
            }
            continue;
        }
        let name: String = rest
            .chars()
            .take_while(char::is_ascii_alphanumeric)
            .flat_map(char::to_lowercase)
            .collect();
        if !name.is_empty() && !VOID_TAGS.contains(&name.as_str()) {
            balance += 1;
        }
    }
    balance
}

/// Markdown resumes inside a raw block, so a `##` in a FAQ answer arrives at
/// depth zero and is listed too; the render decides whether it is usable.
fn heading_offsets(markdown: &str) -> Vec<usize> {
    use pulldown_cmark::{Event, HeadingLevel, Tag};
    let mut depth = 0usize;
    let mut heads = Vec::new();
    for (ev, range) in parser(markdown).into_offset_iter() {
        match ev {
            Event::Start(Tag::Heading {
                level: HeadingLevel::H2,
                ..
            }) => {
                if depth == 0 {
                    heads.push(range.start);
                }
                depth += 1;
            }
            Event::Start(_) => depth += 1,
            Event::End(_) => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    heads
}

/// `None` when the split would land inside an element the renderer left open.
fn band_at(markdown: &str, at: usize) -> Option<(String, usize)> {
    let marked = format!("{}\n\n{BAND_MARK}\n\n{}", &markdown[..at], &markdown[at..]);
    let html = render(&marked);
    let i = html.find(BAND_MARK)?;
    // The renderer's own newline would otherwise sit between the offset and
    // the heading it points at.
    let rest = html[i + BAND_MARK.len()..].trim_start();
    let joined = format!("{}{rest}", &html[..i]);
    (tag_balance(&joined[..i]) == 0).then_some((joined, i))
}

/// Marking the Markdown and cutting the mark out afterwards keeps the offset on
/// a block boundary the renderer chose, rather than one guessed from serialised
/// HTML. Sanitised output is the only judge, so a heading nested in raw markup
/// moves the band down rather than costing the post one.
fn render_with_band(markdown: &str) -> (String, Option<usize>) {
    // A post writing the marker would capture the split point.
    if markdown.contains(BAND_CLASS) {
        return (render(markdown), None);
    }
    let heads = heading_offsets(markdown);
    // Not the first, and not the last: the band needs a heading after it to
    // reach readers who stop before the closing CTA.
    let candidates = heads
        .get(1..heads.len().saturating_sub(1))
        .unwrap_or_default();
    for &at in candidates {
        if let Some((html, i)) = band_at(markdown, at) {
            return (html, Some(i));
        }
    }
    (render(markdown), None)
}

/// A figure counts as embedded only where the sanitiser left a real element.
/// A post that writes the class name in prose or a code fence gets its angle
/// brackets escaped, so the name alone must not pull in a script.
fn has_mount(body_html: &str, mount: &str) -> bool {
    body_html.contains(&format!("<div class=\"{mount}\""))
}

/// Whether a real element carries `class` as one of its class tokens. The bare
/// name also matches escaped prose, and a whole-attribute match misses the
/// class sharing the attribute with another.
#[cfg(test)]
fn carries_class(html: &str, class: &str) -> bool {
    html.split("class=\"")
        .skip(1)
        .filter_map(|rest| rest.split('"').next())
        .any(|list| list.split_whitespace().any(|c| c == class))
}

/// Scripts this post needs, read back off the rendered body: an embed exists
/// only if its mount survived sanitising, so a stripped figure cannot leave a
/// script loading against nothing.
fn embed_scripts(body_html: &str) -> Vec<&'static str> {
    EMBEDS
        .iter()
        .filter(|(mount, _)| has_mount(body_html, mount))
        .map(|(_, script)| *script)
        .collect()
}

/// Shared so an extension can't apply to the render but not the sitemap.
fn parser(markdown: &str) -> pulldown_cmark::Parser<'_> {
    let mut opts = pulldown_cmark::Options::empty();
    opts.insert(pulldown_cmark::Options::ENABLE_TABLES);
    opts.insert(pulldown_cmark::Options::ENABLE_STRIKETHROUGH);
    pulldown_cmark::Parser::new_ext(markdown, opts)
}

/// Mirrors pulldown-cmark's `raw_text`, which fills the rendered `alt`:
/// everything up to the matching close folds into one string, so a nested
/// image belongs to its parent's alt rather than standing on its own.
/// Remote images are not ours to submit; alt-less ones have nothing to say.
fn collect_images(markdown: &str) -> Vec<PostImage> {
    use pulldown_cmark::{Event, Tag};

    let mut out: Vec<PostImage> = Vec::new();
    let mut open: Option<(String, String, usize)> = None;
    for ev in parser(markdown) {
        let Some((_, alt, depth)) = open.as_mut() else {
            if let Event::Start(Tag::Image { dest_url, .. }) = ev {
                open = Some((dest_url.into_string(), String::new(), 0));
            }
            continue;
        };
        match ev {
            Event::Start(_) => *depth += 1,
            Event::End(_) if *depth > 0 => *depth -= 1,
            Event::End(_) => {
                let (path, alt, _) = open.take().expect("matched above");
                let alt = alt.trim().to_string();
                if path.starts_with("/static/") && !alt.is_empty() {
                    out.push(PostImage { path, alt });
                }
            }
            Event::Text(t) | Event::Code(t) | Event::InlineHtml(t) => alt.push_str(&t),
            Event::SoftBreak | Event::HardBreak | Event::Rule => alt.push(' '),
            _ => {}
        }
    }
    out
}

#[derive(Template, WebTemplate)]
#[template(path = "marketing/blog_index.html")]
struct BlogIndexPage {
    canonical_url: String,
    app_url: String,
    og: OpenGraph,
    blog_ld: JsonLd,
    breadcrumb_ld: JsonLd,
    posts: Vec<PostSummary>,
    version: &'static str,
}

#[derive(Debug, Clone)]
pub struct PostSummary {
    pub slug: String,
    pub title: String,
    pub date: String,
    pub excerpt: String,
    pub tags: Vec<String>,
}

#[derive(Template, WebTemplate)]
#[template(path = "marketing/blog_post.html")]
struct BlogPostPage {
    slug: String,
    canonical_url: String,
    app_url: String,
    og: OpenGraph,
    json_ld: JsonLd,
    item_list_ld: Option<JsonLd>,
    faq_ld: Option<JsonLd>,
    meta_title: String,
    title: String,
    date: String,
    updated: Option<String>,
    author: &'static Author,
    tags: Vec<String>,
    body_before: String,
    body_after: Option<String>,
    start_band_position: &'static str,
    cta_label: Option<String>,
    embed_scripts: Vec<&'static str>,
    related: Vec<RelatedLink>,
    version: &'static str,
}

fn render_index(cfg: &MarketingCfg) -> CachedRender {
    let canonical_url = format!("{}/blog", cfg.canonical_origin);
    let og = OpenGraph::default_for(
        &format!("{BRAND} Blog"),
        &canonical_url,
        &cfg.canonical_origin,
    );
    let published = list_published();
    let ld_posts: Vec<(&str, &str, &str)> = published
        .iter()
        .map(|p| (p.title.as_str(), p.slug.as_str(), p.date.as_str()))
        .collect();
    let blog_ld = json_ld_blog(&cfg.canonical_origin, &ld_posts);
    let breadcrumb_ld = json_ld_breadcrumb(&cfg.canonical_origin, "Blog", "/blog");
    let posts: Vec<PostSummary> = published
        .into_iter()
        .map(|p| PostSummary {
            slug: p.slug.clone(),
            title: p.title.clone(),
            date: p.date.clone(),
            excerpt: p.excerpt.clone(),
            tags: p.tags.clone(),
        })
        .collect();
    let body = BlogIndexPage {
        canonical_url,
        app_url: cfg.app_url.clone(),
        og,
        blog_ld,
        breadcrumb_ld,
        posts,
        version: env!("CARGO_PKG_VERSION"),
    }
    .render()
    .unwrap_or_else(|e| format!("<!-- blog index render failed: {e} -->"));
    cached_render(body)
}

/// A cross-link surfaced beneath a post to keep readers on-site and
/// spread internal link equity.
#[derive(Debug, Clone)]
pub struct RelatedLink {
    pub slug: String,
    pub title: String,
    pub date: String,
}

/// Rank other published posts by shared-tag overlap, newest first as the
/// tiebreak, backfilling with recent posts so every article links out
/// even when its tags are unique.
fn related_posts(post: &Post, limit: usize) -> Vec<RelatedLink> {
    let mut scored: Vec<(usize, &'static Post)> = list_published()
        .into_iter()
        .filter(|p| p.slug != post.slug)
        .map(|p| {
            let overlap = p.tags.iter().filter(|t| post.tags.contains(t)).count();
            (overlap, p)
        })
        .collect();
    scored.sort_by_key(|&(overlap, _)| std::cmp::Reverse(overlap));
    scored
        .into_iter()
        .take(limit)
        .map(|(_, p)| RelatedLink {
            slug: p.slug.clone(),
            title: p.title.clone(),
            date: p.date.clone(),
        })
        .collect()
}

fn render_post(cfg: &MarketingCfg, post: &Post) -> CachedRender {
    let canonical_url = format!("{}/blog/{}", cfg.canonical_origin, post.slug);
    let og = OpenGraph::for_post(
        &cfg.canonical_origin,
        &post.title,
        &post.excerpt,
        &post.slug,
        post.og_image.as_deref(),
    );
    let date_modified = post.updated.as_deref().unwrap_or(&post.date);
    let json_ld = json_ld_blog_posting(
        &cfg.canonical_origin,
        &post.title,
        &post.excerpt,
        &post.slug,
        &post.date,
        date_modified,
        &og.image,
    );
    let item_list_ld = (!post.list_items.is_empty()).then(|| {
        json_ld_item_list(
            &cfg.canonical_origin,
            &post.slug,
            &post.title,
            &post.list_items,
        )
    });
    let faq_ld = (!post.faqs.is_empty()).then(|| {
        let pairs: Vec<(&str, &str)> = post
            .faqs
            .iter()
            .map(|(q, a)| (q.as_str(), a.as_str()))
            .collect();
        json_ld_faqpage(&pairs)
    });
    let (body_before, body_after) = match post.band_at.filter(|_| post.cta_label.is_some()) {
        Some(at) => (
            post.body_html[..at].to_string(),
            Some(post.body_html[at..].to_string()),
        ),
        None => (post.body_html.clone(), None),
    };
    let body = BlogPostPage {
        slug: post.slug.clone(),
        canonical_url,
        app_url: cfg.app_url.clone(),
        og,
        json_ld,
        item_list_ld,
        faq_ld,
        meta_title: post
            .meta_title
            .clone()
            .unwrap_or_else(|| post.title.clone()),
        title: post.title.clone(),
        date: post.date.clone(),
        updated: post.updated.clone().filter(|u| u != &post.date),
        author: &AUTHOR,
        tags: post.tags.clone(),
        body_before,
        body_after,
        start_band_position: "blog-band",
        cta_label: post.cta_label.clone(),
        embed_scripts: post.embed_scripts.clone(),
        related: related_posts(post, RELATED_LIMIT),
        version: env!("CARGO_PKG_VERSION"),
    }
    .render()
    .unwrap_or_else(|e| format!("<!-- blog post render failed: {e} -->"));
    cached_render(body)
}

pub async fn index(State(cfg): State<Arc<MarketingCfg>>, headers: HeaderMap) -> Response {
    let cached = INDEX_CACHED.get_or_init(|| render_index(&cfg));
    serve_cached(&headers, cached, &INDEX_CACHE_CONTROL)
}

pub async fn post(
    State(cfg): State<Arc<MarketingCfg>>,
    Path(slug): Path<String>,
    headers: HeaderMap,
) -> Response {
    let cache = RENDERED_INDEX.get_or_init(|| build_post_index(&cfg));
    match cache.get(&slug) {
        Some(cached) => {
            let source = SOURCES
                .get_or_init(build_sources)
                .get(&slug)
                .expect("same post list");
            super::negotiate::serve(&headers, cached, source, &POST_CACHE_CONTROL)
        }
        None => not_found_page(&cfg),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Asserts against the rendered `alt`, the only description that counts.
    #[track_caller]
    fn assert_alt_matches_render(markdown: &str, expected: &str) {
        let images = collect_images(markdown);
        assert_eq!(images.len(), 1, "{markdown:?} -> {images:?}");
        assert_eq!(images[0].alt, expected);
        assert!(
            render(markdown).contains(&format!("alt=\"{expected}\"")),
            "rendered alt disagrees: {}",
            render(markdown)
        );
    }

    #[test]
    fn image_alt_folds_a_line_break_into_a_space() {
        assert_alt_matches_render("![line one\nline two](/static/a.webp)", "line one line two");
    }

    #[test]
    fn image_alt_drops_strikethrough_markers() {
        assert_alt_matches_render("![before ~~after~~](/static/a.webp)", "before after");
    }

    #[test]
    fn nested_image_folds_into_its_parents_alt() {
        let images = collect_images("![outer ![inner](/static/b.webp) tail](/static/a.webp)");
        assert_eq!(
            images.len(),
            1,
            "the inner image is alt text, not an image of its own: {images:?}"
        );
        assert_eq!(images[0].path, "/static/a.webp");
        assert_eq!(images[0].alt, "outer inner tail");
    }

    #[test]
    fn collect_images_skips_remote_and_alt_less_images() {
        let images = collect_images(
            "![hosted elsewhere](https://example.com/a.webp)\n\n![](/static/b.webp)\n\n![kept](/static/c.webp)",
        );
        let paths: Vec<&str> = images.iter().map(|i| i.path.as_str()).collect();
        assert_eq!(paths, ["/static/c.webp"]);
    }

    #[test]
    fn og_image_assets_exist_at_social_card_size() {
        for post in load_posts() {
            let Some(img) = &post.og_image else { continue };
            let rel = img.strip_prefix('/').unwrap();
            let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(rel);
            let bytes = std::fs::read(&path)
                .unwrap_or_else(|e| panic!("{}: og_image {img} unreadable: {e}", post.slug));
            assert_eq!(
                &bytes[..8],
                b"\x89PNG\r\n\x1a\n",
                "{}: og_image must be a PNG",
                post.slug
            );
            let w = u32::from_be_bytes(bytes[16..20].try_into().unwrap());
            let h = u32::from_be_bytes(bytes[20..24].try_into().unwrap());
            assert_eq!((w, h), (1200, 630), "{}: social card size", post.slug);
        }
    }

    #[test]
    fn og_image_outside_static_is_rejected() {
        let raw = "+++\ntitle = \"x\"\ndate = \"2026-01-01\"\nog_image = \"https://cdn.example.com/x.png\"\n+++\nbody";
        assert!(parse_post(raw, "x").is_err());
    }

    #[test]
    fn list_items_appear_in_post_body() {
        for post in load_posts() {
            for item in &post.list_items {
                assert!(
                    post.body_md.contains(item.as_str()),
                    "{}: ItemList entry {item:?} not found in the article body",
                    post.slug
                );
            }
        }
    }

    /// Exact on sanitised output, which is what `render_with_band` gates on.
    #[test]
    fn an_unbalanced_render_drops_the_band() {
        assert_eq!(tag_balance("<div><p>text</p>"), 1);
        assert_eq!(tag_balance("<div><p>text</p></div>"), 0);
        assert_eq!(tag_balance("<p>a<br>b</p><img src=\"x\">"), 0);
        assert_eq!(tag_balance("&lt;div&gt; escaped text"), 0);
    }

    /// The class survives sanitising, so post content can reach the split.
    #[test]
    fn a_post_writing_the_marker_class_gets_no_band() {
        let md = concat!(
            "intro\n\n<div class=\"mk-band\"></div>\n\n",
            "## One\n\na\n\n## Two\n\nb\n\n## Three\n\nc\n",
        );
        let (html, band) = render_with_band(md);
        assert_eq!(band, None, "a post that writes the marker gets no band");
        assert!(
            !html.contains(BAND_MARK) || html.matches(BAND_MARK).count() == 1,
            "the injected mark must not be added on top of the author's"
        );
    }

    /// Posts that carry a CTA, and so the ones that must also carry a band.
    const IN_MARKET: &[&str] = &[
        "uptime-sla",
        "statuspage-alternatives",
        "is-98-uptime-good",
        "how-much-downtime-is-99-9-uptime",
        "how-much-downtime-is-99-95-uptime",
        "how-much-downtime-is-99-99-uptime",
        "best-self-hosted-uptime-monitoring-tools",
        "pingdom-alternatives",
        "do-i-need-an-uptime-monitor",
        "cron-jobs-fail-silently",
        "uptime-kuma-rest-api",
    ];

    #[test]
    fn only_in_market_posts_carry_a_cta() {
        for post in load_posts() {
            match &post.cta_label {
                Some(label) => {
                    assert!(!label.trim().is_empty(), "{}: blank cta_label", post.slug);
                    assert!(
                        IN_MARKET.contains(&post.slug.as_str()),
                        "{}: carries a CTA but is not on the in-market list; add it \
                         deliberately or drop the cta_label",
                        post.slug
                    );
                }
                None => assert!(
                    !IN_MARKET.contains(&post.slug.as_str()),
                    "{}: listed as in-market but has no cta_label",
                    post.slug
                ),
            }
        }
    }

    /// Asserts the band's own contract on the way past.
    fn banded_heading(md: &str) -> Option<String> {
        let (html, at) = render_with_band(md);
        let at = at?;
        assert_eq!(tag_balance(&html[..at]), 0, "band splits an open element");
        let rest = &html[at..];
        assert!(
            rest.starts_with("<h2"),
            "band is not at a heading: {rest:.40}"
        );
        let inner = rest
            .split_once('>')
            .unwrap()
            .1
            .split_once("</h2>")
            .unwrap()
            .0;
        Some(strip_tags(inner))
    }

    fn strip_tags(html: &str) -> String {
        let mut out = String::new();
        let mut inside = false;
        for c in html.chars() {
            match c {
                '<' => inside = true,
                '>' => inside = false,
                _ if !inside => out.push(c),
                _ => {}
            }
        }
        out.trim().to_string()
    }

    #[test]
    fn the_band_lands_before_the_first_heading_that_survives_rendering() {
        let details = concat!(
            "intro\n\n## One\n\ntext\n\n",
            "<details class=\"mk-faq\">\n<summary>Q</summary>\n<div class=\"mk-faq__body\">\n\n",
            "## Nested\n\nanswer\n\n</div>\n</details>\n\n",
            "## Two\n\nmore\n\n## Three\n\nend\n",
        );
        let blockquote = concat!(
            "intro\n\n## One\n\ntext\n\n",
            "<blockquote>\n\n## Quoted\n\nsaid\n\n</blockquote>\n\n",
            "## Two\n\nmore\n\n## Three\n\nend\n",
        );
        let cases: [(&str, &str, &str); 5] = [
            ("plain", "lede\n\n## a\n\none\n\n## b\n\ntwo\n\n## c\n", "b"),
            (
                "markdown container",
                "lede\n\n## a\n\n- item\n\n  ## nested\n\n## b\n\nx\n\n## c\n",
                "b",
            ),
            (
                "phantom tag in alt text",
                "![the <h2> outline](/static/marketing/x.webp)\n\n## a\n\none\n\n## b\n\ntwo\n\n## c\n",
                "b",
            ),
            ("raw details block", details, "Two"),
            ("raw blockquote", blockquote, "Two"),
        ];
        for (name, md, want) in cases {
            assert_eq!(
                banded_heading(md).as_deref(),
                Some(want),
                "{name}: band landed on the wrong heading"
            );
        }
    }

    #[test]
    fn a_short_post_gets_no_mid_article_band() {
        assert!(
            render_with_band("lede\n\n## a\n\none\n\n## b\n")
                .1
                .is_none(),
            "two headings leaves no heading after the band"
        );
    }

    #[test]
    fn carries_class_reads_tokens_not_substrings() {
        assert!(carries_class(r#"<div class="mk-band"></div>"#, BAND_CLASS));
        assert!(carries_class(
            r#"<div class="mk-band mk-table-scroll">"#,
            BAND_CLASS
        ));
        // A post may name the class in prose; that is not the band rendering.
        assert!(!carries_class("<p>the mk-band class</p>", BAND_CLASS));
        assert!(!carries_class(r#"<div class="mk-bandit">"#, BAND_CLASS));
    }

    /// `render_with_band` drops the band on a non-zero prefix, so a sign error
    /// here silently un-bands a post rather than failing anything.
    #[test]
    fn tag_balance_counts_a_void_tag_the_same_however_it_is_written() {
        assert_eq!(tag_balance("<div></div>"), 0);
        assert_eq!(tag_balance("<div>"), 1);
        assert_eq!(tag_balance("</div>"), -1);
        for void in ["br", "embed", "source", "track", "link", "meta", "img"] {
            assert_eq!(tag_balance(&format!("<{void}>")), 0, "<{void}>");
            assert_eq!(
                tag_balance(&format!("<{void}></{void}>")),
                0,
                "<{void}> pair"
            );
        }
    }

    #[test]
    fn the_band_mark_is_cut_out_and_leaves_a_heading_boundary() {
        for post in all() {
            // Token inside a real `class` attribute: the bare name also matches
            // prose, which a post is allowed to write, and an exact
            // `class="mk-band"` misses the class sharing an attribute.
            assert!(
                !carries_class(&post.body_html, BAND_CLASS),
                "{}: band mark survived into the body",
                post.slug
            );
            if let Some(at) = post.band_at {
                assert!(
                    post.body_html[at..].starts_with("<h2"),
                    "{}: band lands mid-element",
                    post.slug
                );
            }
        }
    }

    /// The include is resolved by name, so assert on rendered HTML — the unit
    /// tests above still pass if it is renamed away.
    #[test]
    fn a_rendered_post_carries_exactly_one_start_band() {
        let cfg = MarketingCfg {
            app_url: "https://app.uptimepage.dev".into(),
            canonical_origin: "https://uptimepage.dev".into(),
            blog_enabled: true,
            mcp_url: None,
            checkout_open: false,
            trusted_proxies: Vec::new(),
        };
        let mut rendered = 0usize;
        for post in all().iter().filter(|p| !p.draft) {
            let html = String::from_utf8(render_post(&cfg, post).body.to_vec()).expect("utf8");
            let bands = html
                .matches(r#"data-umami-event-position="blog-band""#)
                .count();
            // `band_at` is computed for every post; the template gates on the CTA.
            let want = usize::from(post.cta_label.is_some() && post.band_at.is_some());
            assert_eq!(bands, want, "{}: wrong number of start bands", post.slug);
            rendered += bands;
        }
        // Deriving `want` from `band_at` alone passes when placement stops
        // working: every post wants zero and every post gets zero. Pinned to
        // the in-market list so losing one post trips it, not just losing all.
        assert_eq!(
            rendered,
            IN_MARKET.len(),
            "a published in-market post stopped rendering its band"
        );
    }

    /// A parse failure only logs, so a broken post disappears from the site
    /// while every other test still passes. Counting is the cheap tripwire;
    /// the dropped file's name is in the error log.
    #[test]
    fn every_markdown_file_parses_into_a_post() {
        let files = BLOG_DIR
            .files()
            .filter(|f| f.path().extension().and_then(|e| e.to_str()) == Some("md"))
            .count();
        assert_eq!(
            files,
            load_posts().len(),
            "a markdown file did not parse into a post"
        );
    }

    #[test]
    fn faq_front_matter_mirrors_visible_answers() {
        for post in load_posts() {
            for (q, a) in &post.faqs {
                assert!(!q.is_empty() && !a.is_empty(), "{}: empty FAQ", post.slug);
                // Answers may flatten inline links, so match the opening
                // sentence rather than the whole string: catches drift
                // between schema and the visible copy without false negatives.
                let lead = a.split(['.', '?', '!']).next().unwrap_or(a).trim();
                assert!(
                    post.body_md.contains(lead),
                    "{}: FAQ answer {q:?} does not track the visible copy; \
                     schema would mismatch the page",
                    post.slug
                );
            }
        }
    }

    #[test]
    fn renderer_wraps_tables_in_scroll_div() {
        let html = render("| a |\n| - |\n| 1 |");
        let open = html.find("mk-table-scroll");
        assert!(html.contains("tabindex"), "got: {html}");
        let table_end = html.find("</table>");
        assert!(open.is_some() && table_end.is_some(), "got: {html}");
        assert!(open < table_end, "got: {html}");
        assert!(html[table_end.unwrap()..].contains("</div>"), "got: {html}");
    }

    #[test]
    fn renderer_keeps_faq_accordion_and_parses_inner_links() {
        let md = "<details class=\"mk-faq\">\n<summary>Q</summary>\n<div class=\"mk-faq__body\">\n\nAnswer [ref](https://example.com/x).\n\n</div>\n</details>";
        let html = render(md);
        assert!(
            html.contains("<details class=\"mk-faq\">"),
            "details dropped: {html}"
        );
        assert!(html.contains("<summary>"), "summary dropped: {html}");
        assert!(html.contains("mk-faq__body"), "body class dropped: {html}");
        assert!(
            html.contains("href=\"https://example.com/x\""),
            "inner markdown link not parsed: {html}"
        );
    }

    #[test]
    fn renderer_strips_script_tag() {
        let html = render("<script>alert(1)</script>");
        assert!(!html.contains("<script"), "got: {html}");
        assert!(!html.to_lowercase().contains("alert"));
    }

    #[test]
    fn renderer_strips_onerror() {
        let html = render("![x](javascript:1)\n\n<img src=x onerror=alert(1)>");
        assert!(!html.contains("onerror"), "got: {html}");
        assert!(!html.contains("javascript:"), "got: {html}");
    }

    #[test]
    fn renderer_strips_iframe_and_svg_onload() {
        let html = render("<iframe src=//evil></iframe>\n<svg onload=alert(1)></svg>");
        assert!(!html.contains("<iframe"), "got: {html}");
        assert!(!html.contains("onload"), "got: {html}");
    }

    #[test]
    fn renderer_keeps_safe_link() {
        let html = render("[hi](https://example.com)");
        assert!(html.contains("href=\"https://example.com"));
        assert!(html.contains("rel=\"noopener noreferrer\""));
    }

    #[test]
    fn split_front_matter_extracts_block() {
        let raw = "+++\ntitle = \"x\"\ndate = \"2026-05-20\"\n+++\nbody\n";
        let (front, body) = split_front_matter(raw).expect("front matter");
        assert!(front.contains("title"));
        assert!(body.starts_with("body"));
    }

    #[test]
    fn parse_post_succeeds_on_minimal_doc() {
        let raw = "+++\ntitle = \"Hi\"\ndate = \"2026-05-20\"\n+++\nbody\n";
        let post = parse_post(raw, "hi").expect("parse");
        assert_eq!(post.slug, "hi");
        assert_eq!(post.title, "Hi");
        assert_eq!(post.meta_title, None);
        assert!(!post.draft);
    }

    #[test]
    fn parse_post_accepts_meta_title_override() {
        let raw = "+++\ntitle = \"Visible title\"\nmeta_title = \"Search title\"\ndate = \"2026-05-20\"\n+++\nbody\n";
        let post = parse_post(raw, "hi").expect("parse");
        assert_eq!(post.title, "Visible title");
        assert_eq!(post.meta_title.as_deref(), Some("Search title"));
    }

    #[test]
    fn posts_fit_serp_limits() {
        for p in all() {
            let search_title = p.meta_title.as_deref().unwrap_or(&p.title);
            assert!(
                search_title.len() <= 65,
                "{}: title {} chars > 65",
                p.slug,
                search_title.len()
            );
            assert!(
                p.excerpt.len() <= 160,
                "{}: excerpt {} chars > 160",
                p.slug,
                p.excerpt.len()
            );
        }
    }

    #[test]
    fn every_post_links_out_to_related() {
        let posts = list_published();
        let others = posts.len().saturating_sub(1);
        if others == 0 {
            return;
        }
        let want = RELATED_LIMIT.min(others);
        for p in &posts {
            let related = related_posts(p, RELATED_LIMIT);
            assert_eq!(
                related.len(),
                want,
                "{}: expected {want} related links, got {}",
                p.slug,
                related.len()
            );
            assert!(
                related.iter().all(|r| r.slug != p.slug),
                "{}: related links must not include the post itself",
                p.slug
            );
        }
    }

    #[test]
    fn posts_load_and_sort_by_date_desc() {
        let posts = all();
        if posts.is_empty() {
            return;
        }
        for w in posts.windows(2) {
            assert!(w[0].date >= w[1].date, "blog index must be date-desc");
        }
    }

    #[test]
    fn embed_mount_survives_sanitising() {
        for (mount, _) in EMBEDS {
            let html = render(&format!("<div class=\"{mount}\"></div>"));
            assert!(
                html.contains(mount),
                "{mount} is stripped, so its figure would never mount: {html}"
            );
        }
    }

    /// An unknown asset path resolves to a bare `/static/...` that 404s at
    /// runtime, so a typo here would be a silently dead figure.
    #[test]
    fn embed_scripts_are_built_assets() {
        for (mount, script) in EMBEDS {
            assert_ne!(
                crate::web::assets::url(script),
                format!("/static/{script}"),
                "{mount} loads {script}, which is not a built bundle"
            );
        }
    }

    #[test]
    fn a_post_embedding_a_figure_asks_for_its_script() {
        let embedded: Vec<&Post> = all()
            .iter()
            .filter(|p| !p.embed_scripts.is_empty())
            .collect();
        for post in &embedded {
            for script in &post.embed_scripts {
                assert!(
                    EMBEDS.iter().any(|(_, s)| s == script),
                    "{} asks for an unknown script {script}",
                    post.slug
                );
            }
        }
        // The mount only ever appears via Markdown, so a post that writes one
        // must come back out of the parser carrying its script.
        for post in all() {
            for (mount, script) in EMBEDS {
                assert_eq!(
                    has_mount(&post.body_html, mount),
                    post.embed_scripts.contains(script),
                    "{} disagrees about {mount}",
                    post.slug
                );
            }
        }
    }

    #[test]
    fn a_mount_named_in_prose_loads_nothing() {
        for (mount, _) in EMBEDS {
            let html = render(&format!(
                "Write `<div class=\"{mount}\"></div>` to embed it."
            ));
            assert!(
                embed_scripts(&html).is_empty(),
                "{mount} written as text must not load its script: {html}"
            );
        }
    }
}
