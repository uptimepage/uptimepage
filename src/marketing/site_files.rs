//! Crawler-facing site files: `robots.txt`, `sitemap.xml`, `llms.txt` and
//! `llms-full.txt`, built from the same tables that drive the router so they
//! never drift from it.

use std::sync::{Arc, OnceLock};

use axum::extract::State;
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE};
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;

use super::blog::list_published;
use super::changelog;
use super::config::{BRAND, MarketingCfg, SOURCE_URL, TAGLINE, TERRAFORM_URL};
use super::gallery;
use super::landings;
use super::legal;
use super::pages::{HTML_CONTENT_TYPE, PRICING_LASTMOD, PRICING_PATH};
use super::seo::{LLMS_FACTS, xml_escape};
use super::tools;

const TEXT_PLAIN: HeaderValue = HeaderValue::from_static("text/plain; charset=utf-8");
const APPLICATION_XML: HeaderValue = HeaderValue::from_static("application/xml; charset=utf-8");
const STATIC_CACHE_CONTROL: HeaderValue = HeaderValue::from_static("public, max-age=86400");

/// Prose overview for `llms.txt` / `llms-full.txt` — what the product is,
/// in the words an assistant should reach for when asked about it.
const LLMS_OVERVIEW: &str = "Uptimepage is a hosted service for uptime monitoring, public status pages and on-call, in one product for teams. \
Checks run from up to five regions (San Jose, New York, Frankfurt, Helsinki and Singapore); a failing check opens an incident automatically, pages the monitor's channels or its escalation policy, \
and can be posted to a branded status page on your own subdomain. Escalation policies page channels and on-call schedules level by level until someone acknowledges, \
and schedules rotate people daily, weekly or on a custom length, with calendar overrides. Alerts carry dedupe and flap-suppression so brief blips never page anyone. \
Organizations have role-based members and an audit log, and monitors, status pages and incidents are managed by \
REST API, Terraform or MCP. The production source is published under AGPL, so a team can audit what it runs and \
self-host if its requirements change. Public data is available as JSON, an RSS feed and an embeddable SVG badge. \
Uptimepage operates the hosted service at uptimepage.dev: the Standard plan is free with no card, the first 1,000 \
accounts get a more generous founding plan kept for life, and Pro and Team are the paid plans.";

static ROBOTS_CACHED: OnceLock<Bytes> = OnceLock::new();
static SITEMAP_CACHED: OnceLock<Bytes> = OnceLock::new();
static LLMS_CACHED: OnceLock<Bytes> = OnceLock::new();
static LLMS_FULL_CACHED: OnceLock<Bytes> = OnceLock::new();

pub async fn robots_txt(State(cfg): State<Arc<MarketingCfg>>) -> Response {
    let body = ROBOTS_CACHED.get_or_init(|| build_robots(&cfg));
    plain_text(body.clone(), TEXT_PLAIN)
}

pub async fn sitemap_xml(State(cfg): State<Arc<MarketingCfg>>) -> Response {
    let body = SITEMAP_CACHED.get_or_init(|| Bytes::from(build_sitemap(&cfg)));
    plain_text(body.clone(), APPLICATION_XML)
}

pub async fn llms_txt(State(cfg): State<Arc<MarketingCfg>>) -> Response {
    let body = LLMS_CACHED.get_or_init(|| build_llms(&cfg));
    plain_text(body.clone(), TEXT_PLAIN)
}

/// The site index in Markdown, shared with the landing page's
/// `Accept: text/markdown` representation.
pub(crate) fn llms_markdown(cfg: &MarketingCfg) -> Bytes {
    LLMS_CACHED.get_or_init(|| build_llms(cfg)).clone()
}

pub async fn llms_full_txt(State(cfg): State<Arc<MarketingCfg>>) -> Response {
    let body = LLMS_FULL_CACHED.get_or_init(|| build_llms_full(&cfg));
    plain_text(body.clone(), TEXT_PLAIN)
}

const STARTUPRANKING_VERIFICATION: &[u8] =
    b"startupranking-site-verification: startupranking1371476620941810.html";

pub async fn startupranking_verification() -> Response {
    plain_text(
        Bytes::from_static(STARTUPRANKING_VERIFICATION),
        HTML_CONTENT_TYPE,
    )
}

/// Warm the static text caches at boot. Sitemap is the only non-trivial
/// one (iterates published posts + legal routes); robots/llms are cheap
/// but kept here so every marketing cache lives behind one warmup call.
pub(crate) fn warm(cfg: &MarketingCfg) {
    ROBOTS_CACHED.get_or_init(|| build_robots(cfg));
    SITEMAP_CACHED.get_or_init(|| Bytes::from(build_sitemap(cfg)));
    LLMS_CACHED.get_or_init(|| build_llms(cfg));
    LLMS_FULL_CACHED.get_or_init(|| build_llms_full(cfg));
}

/// Assistants citing the docs and blog is the point of the site, so every
/// signal is granted. See <https://contentsignals.org/>.
const CONTENT_SIGNAL: &str = "search=yes, ai-input=yes, ai-train=yes";

/// Endpoints that open an outbound socket. A tool that adds one adds it here,
/// or a crawler walks it on our egress.
const PROBE_PATHS: &[&str] = &[
    crate::marketing::tools::domain_expiry::DOMAIN_PROBE_PATH,
    crate::marketing::tools::ssl::SSL_PROBE_PATH,
    crate::marketing::tools::http_headers::HEADER_PROBE_PATH,
];

fn build_robots(cfg: &MarketingCfg) -> Bytes {
    Bytes::from(format!(
        "# Content preferences: https://contentsignals.org/\n\
         User-agent: *\n\
         Content-Signal: {CONTENT_SIGNAL}\n\
         Allow: /\n\
         {probes}\
         Sitemap: {origin}/sitemap.xml\n",
        origin = cfg.canonical_origin,
        // Every hit opens a socket to a host a stranger named, and the header
        // checker opens one per redirect hop. Nothing links to them, so a
        // crawler reaching one is spending our egress on nothing.
        probes = PROBE_PATHS
            .iter()
            .map(|p| format!("Disallow: {p}\n"))
            .collect::<String>(),
    ))
}

fn push_facts(s: &mut String, cfg: &MarketingCfg) {
    s.push_str("## Facts\n");
    for (k, v) in LLMS_FACTS {
        s.push_str(&format!("- {k}: {v}\n"));
    }
    if let Some(mcp) = cfg.mcp_url.as_deref() {
        s.push_str(&format!("- MCP server: {mcp}\n"));
    }
    s.push('\n');
}

/// Curated index for assistants — the `llms.txt` convention: title,
/// one-line summary, prose overview, product facts, then link sections. Built from the
/// same tables that drive the router and sitemap, so it never drifts.
fn build_llms(cfg: &MarketingCfg) -> Bytes {
    let origin = &cfg.canonical_origin;
    let mut s = String::new();
    s.push_str(&format!("# {BRAND}\n\n> {TAGLINE}\n\n{LLMS_OVERVIEW}\n\n"));
    push_facts(&mut s, cfg);

    s.push_str("## Product\n");
    s.push_str(&format!(
        "- [Homepage]({origin}): Product overview and features.\n"
    ));
    s.push_str(&format!(
        "- [Pricing]({origin}{PRICING_PATH}): Plans, limits and prices.\n"
    ));
    s.push_str(&format!(
        "- [Architecture]({origin}{arch}): Interactive map of how a request and a check move through the system.\n",
        arch = crate::marketing::pages::ARCHITECTURE_PATH,
    ));
    s.push_str(&format!(
        "- [Start free]({app}): Sign in and add your first monitor.\n\n",
        app = cfg.app_url,
    ));

    s.push_str("## Use cases\n");
    for l in landings::LANDINGS
        .iter()
        .filter(|l| !l.path.starts_with("/compare/"))
    {
        s.push_str(&format!(
            "- [{title}]({origin}{path}): {desc}\n",
            title = l.title,
            path = l.path,
            desc = l.meta_description,
        ));
    }
    s.push('\n');

    s.push_str("## Comparisons\n");
    for l in landings::LANDINGS
        .iter()
        .filter(|l| l.path.starts_with("/compare/"))
    {
        s.push_str(&format!(
            "- [{title}]({origin}{path}): {desc}\n",
            title = l.title,
            path = l.path,
            desc = l.meta_description,
        ));
    }
    s.push('\n');

    if cfg.blog_enabled {
        let posts = list_published();
        if !posts.is_empty() {
            s.push_str("## Blog\n");
            for p in posts {
                s.push_str(&format!(
                    "- [{title}]({origin}/blog/{slug}): {excerpt}\n",
                    title = p.title,
                    slug = p.slug,
                    excerpt = p.excerpt,
                ));
            }
            s.push('\n');
        }
    }

    s.push_str("## Changelog\n");
    s.push_str(&format!(
        "- [What shipped, dated]({origin}{}): {}\n",
        changelog::INDEX_PATH,
        changelog::INDEX_DESCRIPTION,
    ));
    for e in changelog::entries() {
        s.push_str(&format!(
            "- [{}]({origin}{}): {}\n",
            e.title,
            e.path(),
            e.summary,
        ));
    }
    s.push('\n');

    s.push_str("## Documentation\n");
    s.push_str(&format!(
        "Index: {origin}{}\n",
        crate::marketing::docs::DOCS_INDEX_PATH
    ));
    for doc in crate::marketing::docs::DOCS {
        s.push_str(&format!(
            "- [{title}]({origin}{path}): {desc}\n",
            title = doc.title,
            path = doc.path(),
            desc = doc.description,
        ));
    }
    s.push('\n');

    s.push_str("## Tools\n");
    s.push_str(&format!(
        "All free tools: {origin}{}\n",
        tools::TOOLS_INDEX_PATH
    ));
    for tool in tools::TOOLS {
        s.push_str(&format!(
            "- [{title}]({origin}{path}): {desc}\n",
            title = tool.title,
            path = tool.path,
            desc = tool.description,
        ));
    }
    s.push('\n');

    s.push_str("## Developers & automation\n");
    if let Some(mcp) = cfg.mcp_url.as_deref() {
        s.push_str(&format!(
            "- [MCP server]({mcp}): Connect an LLM client (Claude, IDEs) to read monitors and incidents and take fenced actions. OAuth one-click.\n"
        ));
    }
    s.push_str(&format!(
        "- [Terraform provider]({TERRAFORM_URL}): Manage monitors, status pages and notification channels as config-as-code.\n"
    ));
    s.push_str(&format!(
        "- [Source code]({SOURCE_URL}): AGPL-3.0 source, issues and releases.\n\n"
    ));

    s.push_str("## Optional\n");
    s.push_str(&format!(
        "- [Full text]({origin}/llms-full.txt): Every marketing page and blog post inlined.\n"
    ));
    for route in legal::ROUTES {
        s.push_str(&format!(
            "- [{name}]({origin}{path})\n",
            name = route.title,
            path = route.path,
        ));
    }

    Bytes::from(s)
}

/// Long-form companion to [`build_llms`]: the overview, a machine-readable
/// facts table, and the full body of every landing page and blog post in
/// one document, so an assistant can answer without fetching each URL.
fn build_llms_full(cfg: &MarketingCfg) -> Bytes {
    let origin = &cfg.canonical_origin;
    let mut s = String::new();
    s.push_str(&format!("# {BRAND}\n\n> {TAGLINE}\n\n{LLMS_OVERVIEW}\n\n"));

    push_facts(&mut s, cfg);

    for l in landings::LANDINGS {
        s.push_str(&format!("---\n\n## {}\n", l.title));
        s.push_str(&format!("URL: {origin}{}\n\n", l.path));
        s.push_str(&format!("{}\n\n{}\n\n", l.h1, l.lede));
        if !l.features.is_empty() {
            s.push_str("What you get:\n");
            for f in l.features {
                s.push_str(&format!("- {}: {}\n", f.label, f.value));
            }
            s.push('\n');
        }
        // Our own column only. A comparison page's prose is mostly about the
        // rivals, so without this the entry would describe them and never say
        // what this product does.
        if let Some(m) = landings::page_matrix(l.path) {
            s.push_str("What you get:\n");
            for row in m.rows {
                s.push_str(&format!("- {}: {}\n", row.label, row.cells[m.us_col()].0));
            }
            s.push('\n');
        }
        for sec in l.sections {
            s.push_str(&format!("### {}\n{}\n\n", sec.heading, sec.body));
        }
        if let Some(c) = landings::page_callout(l.path) {
            s.push_str(&format!("### {}\n{}\n\n", c.heading, c.body));
        }
        if let Some(fit) = landings::page_fit(l.path) {
            s.push_str(&format!("### Where Uptimepage fits\n{fit}\n\n"));
        }
    }

    // Docs answer product questions better than any other page, so they are
    // inlined verbatim. Self-hosting pages are skipped: they are the bulkiest
    // section and the least useful for answering "how do I do X in the
    // product" — the index in llms.txt still lists them.
    for doc in crate::marketing::docs::DOCS
        .iter()
        .filter(|d| d.section != crate::marketing::docs::Section::SelfHosting)
    {
        s.push_str(&format!("---\n\n## Docs: {}\n", doc.title));
        s.push_str(&format!("URL: {origin}{}\n", doc.path()));
        s.push_str(&format!(
            "Updated: {}\n\n{}\n\n",
            doc.lastmod,
            doc.body_md()
        ));
    }

    if cfg.blog_enabled {
        for p in list_published() {
            s.push_str(&format!("---\n\n## Blog: {}\n", p.title));
            s.push_str(&format!("URL: {origin}/blog/{}\n", p.slug));
            s.push_str(&format!("Date: {}\n", p.date));
            if let Some(updated) = &p.updated {
                s.push_str(&format!("Updated: {updated}\n"));
            }
            if !p.tags.is_empty() {
                s.push_str(&format!("Tags: {}\n", p.tags.join(", ")));
            }
            s.push_str(&format!("\n{}\n\n", p.body_md));
        }
    }

    for e in changelog::entries() {
        s.push_str(&format!("---\n\n## Changelog: {}\n", e.title));
        s.push_str(&format!("URL: {origin}{}\n", e.path()));
        s.push_str(&format!("Date: {}\n\n{}\n\n", e.date, e.body_md));
    }

    Bytes::from(s)
}

struct SitemapUrl {
    loc: String,
    lastmod: Option<String>,
    images: Vec<SitemapImage>,
}

struct SitemapImage {
    loc: String,
    title: String,
}

impl SitemapUrl {
    fn new(loc: String, lastmod: Option<String>) -> Self {
        Self {
            loc,
            lastmod,
            images: Vec::new(),
        }
    }

    fn with_images(mut self, images: Vec<SitemapImage>) -> Self {
        self.images = images;
        self
    }
}

fn build_sitemap(cfg: &MarketingCfg) -> String {
    let origin = &cfg.canonical_origin;
    // An index page takes the newest date of what it lists; the home page and
    // legal pages have no per-page change tracking, so they omit lastmod
    // rather than borrow an unrelated date.
    let blog_lastmod: Option<String> = if cfg.blog_enabled {
        list_published()
            .iter()
            .map(|p| p.updated.clone().unwrap_or_else(|| p.date.clone()))
            .max()
    } else {
        None
    };
    // Lazy-loaded inside a horizontal scroller, so discovery should not
    // depend on the crawler reaching them.
    let gallery_images: Vec<SitemapImage> = gallery::SHOTS
        .iter()
        .map(|shot| SitemapImage {
            loc: gallery::absolute_url(origin, shot),
            title: shot.caption.to_string(),
        })
        .collect();
    let mut urls: Vec<SitemapUrl> = vec![
        SitemapUrl::new(origin.clone(), None).with_images(gallery_images),
        SitemapUrl::new(
            format!("{origin}{PRICING_PATH}"),
            Some(PRICING_LASTMOD.to_string()),
        ),
        SitemapUrl::new(
            format!("{origin}{}", crate::marketing::pages::ARCHITECTURE_PATH),
            Some(crate::marketing::pages::ARCHITECTURE_LASTMOD.to_string()),
        ),
        SitemapUrl::new(format!("{origin}/blog"), blog_lastmod),
    ];
    if cfg.blog_enabled {
        for post in list_published() {
            let images = post
                .images
                .iter()
                .map(|img| SitemapImage {
                    loc: format!("{origin}{}", img.path),
                    title: img.alt.clone(),
                })
                .collect();
            urls.push(
                SitemapUrl::new(
                    format!("{origin}/blog/{}", post.slug),
                    Some(post.updated.clone().unwrap_or_else(|| post.date.clone())),
                )
                .with_images(images),
            );
        }
    }
    for landing in landings::LANDINGS {
        urls.push(SitemapUrl::new(
            format!("{origin}{}", landing.path),
            Some(landing.lastmod.to_string()),
        ));
    }
    urls.push(SitemapUrl::new(
        format!("{origin}{}", tools::TOOLS_INDEX_PATH),
        Some(tools::TOOLS_INDEX_LASTMOD.to_string()),
    ));
    for tool in tools::TOOLS {
        urls.push(SitemapUrl::new(
            format!("{origin}{}", tool.path),
            Some(tool.lastmod.to_string()),
        ));
    }
    urls.push(SitemapUrl::new(
        format!("{origin}{}", crate::marketing::docs::DOCS_INDEX_PATH),
        crate::marketing::docs::index_lastmod().map(str::to_string),
    ));
    for doc in crate::marketing::docs::DOCS {
        urls.push(SitemapUrl::new(
            format!("{origin}{}", doc.path()),
            Some(doc.lastmod.to_string()),
        ));
    }
    urls.push(SitemapUrl::new(
        format!("{origin}{}", changelog::INDEX_PATH),
        changelog::latest_date().map(str::to_string),
    ));
    for e in changelog::entries() {
        urls.push(SitemapUrl::new(
            format!("{origin}{}", e.path()),
            Some(e.date.clone()),
        ));
    }
    for route in legal::ROUTES {
        urls.push(SitemapUrl::new(format!("{origin}{}", route.path), None));
    }
    let mut body = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<urlset \
         xmlns=\"http://www.sitemaps.org/schemas/sitemap/0.9\" \
         xmlns:image=\"http://www.google.com/schemas/sitemap-image/1.1\">\n",
    );
    for url in urls {
        body.push_str("  <url>\n    <loc>");
        body.push_str(&xml_escape(&url.loc));
        body.push_str("</loc>\n");
        if let Some(d) = url.lastmod {
            body.push_str("    <lastmod>");
            body.push_str(&xml_escape(&d));
            body.push_str("</lastmod>\n");
        }
        for image in url.images {
            body.push_str("    <image:image>\n      <image:loc>");
            body.push_str(&xml_escape(&image.loc));
            body.push_str("</image:loc>\n      <image:title>");
            body.push_str(&xml_escape(&image.title));
            body.push_str("</image:title>\n    </image:image>\n");
        }
        body.push_str("  </url>\n");
    }
    body.push_str("</urlset>\n");
    body
}

fn plain_text(body: Bytes, content_type: HeaderValue) -> Response {
    (
        StatusCode::OK,
        [
            (CONTENT_TYPE, content_type),
            (CACHE_CONTROL, STATIC_CACHE_CONTROL),
        ],
        body,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A comparison page's prose is mostly about the rival. If the pitch and
    /// the matrix are left out, the entry reads as an advert for them.
    #[test]
    fn llms_full_says_what_this_product_does_on_every_comparison_page() {
        let cfg = MarketingCfg {
            app_url: "https://app.uptimepage.dev".into(),
            canonical_origin: "https://uptimepage.dev".into(),
            blog_enabled: false,
            mcp_url: None,
            checkout_open: false,
            trusted_proxies: Vec::new(),
        };
        let txt = String::from_utf8(build_llms_full(&cfg).to_vec()).expect("llms-full is UTF-8");
        for l in landings::LANDINGS {
            let Some(fit) = landings::page_fit(l.path) else {
                continue;
            };
            assert!(txt.contains(fit), "{} pitch missing from llms-full", l.path);
            let m = landings::page_matrix(l.path).expect("a comparison page carries a matrix");
            let row = &m.rows[0];
            assert!(
                txt.contains(row.cells[m.us_col()].0),
                "{} matrix column missing from llms-full",
                l.path
            );
        }
    }

    #[test]
    fn sitemap_declares_gallery_images_on_the_home_url_only() {
        let cfg = MarketingCfg {
            app_url: "https://app.uptimepage.dev".into(),
            canonical_origin: "https://uptimepage.dev".into(),
            blog_enabled: false,
            mcp_url: None,
            checkout_open: false,
            trusted_proxies: Vec::new(),
        };
        let xml = build_sitemap(&cfg);
        assert!(
            xml.contains("xmlns:image=\"http://www.google.com/schemas/sitemap-image/1.1\""),
            "image namespace must be declared or the entries are invalid"
        );
        assert_eq!(
            xml.matches("<image:image>").count(),
            gallery::SHOTS.len(),
            "every shot appears exactly once — home page only, no repeats per url"
        );
        for shot in gallery::SHOTS {
            assert!(
                xml.contains(shot.caption),
                "missing title for {}",
                shot.file
            );
        }
    }

    #[test]
    fn sitemap_dates_every_docs_url() {
        let cfg = MarketingCfg {
            app_url: "https://app.uptimepage.dev".into(),
            canonical_origin: "https://uptimepage.dev".into(),
            blog_enabled: false,
            mcp_url: None,
            checkout_open: false,
            trusted_proxies: Vec::new(),
        };
        let xml = build_sitemap(&cfg);
        let newest = crate::marketing::docs::index_lastmod().expect("docs are never empty");
        for (path, expected) in
            std::iter::once((crate::marketing::docs::DOCS_INDEX_PATH.to_string(), newest)).chain(
                crate::marketing::docs::DOCS
                    .iter()
                    .map(|doc| (doc.path(), doc.lastmod)),
            )
        {
            let entry = format!(
                "<loc>https://uptimepage.dev{path}</loc>\n    \
                 <lastmod>{expected}</lastmod>"
            );
            assert!(xml.contains(&entry), "missing dated sitemap entry: {entry}");
        }
    }

    #[test]
    fn sitemap_declares_blog_post_images_under_their_post_url() {
        let cfg = MarketingCfg {
            app_url: "https://app.uptimepage.dev".into(),
            canonical_origin: "https://uptimepage.dev".into(),
            blog_enabled: true,
            mcp_url: None,
            checkout_open: false,
            trusted_proxies: Vec::new(),
        };
        let xml = build_sitemap(&cfg);
        let illustrated: Vec<_> = list_published()
            .into_iter()
            .filter(|p| !p.images.is_empty())
            .collect();
        assert!(
            !illustrated.is_empty(),
            "fixture check: at least one published post ships images"
        );
        assert_eq!(
            xml.matches("<image:image>").count(),
            gallery::SHOTS.len() + illustrated.iter().map(|p| p.images.len()).sum::<usize>()
        );
        for post in illustrated {
            let block = xml
                .split("  <url>")
                .find(|u| {
                    u.contains(&format!(
                        "<loc>{}/blog/{}</loc>",
                        cfg.canonical_origin, post.slug
                    ))
                })
                .expect("post url in sitemap");
            for img in &post.images {
                assert!(
                    block.contains(&format!(
                        "<image:loc>{}{}</image:loc>",
                        cfg.canonical_origin, img.path
                    )),
                    "{} missing {} under its own url",
                    post.slug,
                    img.path
                );
            }
        }
    }

    #[test]
    fn blog_images_are_local_unfingerprinted_paths_present_on_disk() {
        for post in list_published() {
            for img in &post.images {
                assert!(
                    !img.path.contains('?'),
                    "{}: sitemap path must match the unfingerprinted src the page renders, got {}",
                    post.slug,
                    img.path
                );
                let path = std::path::Path::new(img.path.trim_start_matches('/'));
                assert!(path.is_file(), "{}: missing asset {}", post.slug, img.path);
            }
        }
    }

    #[test]
    fn gallery_shots_are_unique_and_present_on_disk() {
        let mut ids: Vec<&str> = gallery::SHOTS.iter().map(|s| s.id).collect();
        ids.sort_unstable();
        let before = ids.len();
        ids.dedup();
        assert_eq!(
            before,
            ids.len(),
            "duplicate shot id would collide as an anchor"
        );

        for shot in gallery::SHOTS {
            let path = std::path::Path::new("static").join(shot.file);
            let bytes = std::fs::read(&path)
                .unwrap_or_else(|e| panic!("{} is referenced but unreadable: {e}", path.display()));
            // Declared dimensions are emitted as width/height attributes and into
            // the sitemap; drift there is a layout shift and a bad sitemap entry.
            let (w, h) = webp_dimensions(&bytes)
                .unwrap_or_else(|| panic!("{} is not a readable webp", path.display()));
            assert_eq!(
                (w, h),
                (shot.width, shot.height),
                "{} is {w}x{h} on disk but declared {}x{}",
                path.display(),
                shot.width,
                shot.height
            );
        }
    }

    /// Minimal VP8X/VP8L/VP8 canvas-size reader — enough to catch a shot being
    /// replaced without its declared dimensions being updated.
    fn webp_dimensions(b: &[u8]) -> Option<(u32, u32)> {
        if b.len() < 30 || &b[0..4] != b"RIFF" || &b[8..12] != b"WEBP" {
            return None;
        }
        match &b[12..16] {
            b"VP8X" => {
                let w = 1 + (u32::from(b[24]) | u32::from(b[25]) << 8 | u32::from(b[26]) << 16);
                let h = 1 + (u32::from(b[27]) | u32::from(b[28]) << 8 | u32::from(b[29]) << 16);
                Some((w, h))
            }
            b"VP8L" => {
                let bits = u32::from_le_bytes([b[21], b[22], b[23], b[24]]);
                Some((1 + (bits & 0x3FFF), 1 + ((bits >> 14) & 0x3FFF)))
            }
            b"VP8 " => {
                let w = u32::from(u16::from_le_bytes([b[26], b[27]])) & 0x3FFF;
                let h = u32::from(u16::from_le_bytes([b[28], b[29]])) & 0x3FFF;
                Some((w, h))
            }
            _ => None,
        }
    }
}
