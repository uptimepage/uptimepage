//! SEO helpers: per-page OpenGraph + JSON-LD payloads, generated
//! `robots.txt` and `sitemap.xml`, and an optional `llms.txt`.
//!
//! Absolute URLs are mandatory in `og:image`, `og:url`, and `<link
//! rel="canonical">` — social-card scrapers reject relative paths
//! silently — so every URL emitted here is prefixed with
//! `MarketingCfg::canonical_origin`.

use std::borrow::Cow;
use std::sync::{Arc, OnceLock};

use axum::extract::State;
use axum::http::HeaderValue;
use axum::http::StatusCode;
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use serde::Serialize;

use super::blog::list_published;
use super::changelog;
use super::config::{
    AUTHOR, BRAND, CONTACT_EMAIL, META_DESCRIPTION, MarketingCfg, ORG_COUNTRY, ORG_FOUNDING_DATE,
    ORG_LOCALITY, SOURCE_URL, TAGLINE, TERRAFORM_URL,
};
use super::gallery;
use super::landings;
use super::legal;
use super::pages::{APPLICATION_XML, HTML_CONTENT_TYPE, TEXT_PLAIN};
use super::tools;

const STATIC_CACHE_CONTROL: HeaderValue = HeaderValue::from_static("public, max-age=86400");

/// Public profiles that establish the brand entity for search engines.
const ORG_SAME_AS: &[&str] = &[
    "https://github.com/uptimepage",
    SOURCE_URL,
    "https://bsky.app/profile/uptimepage.bsky.social",
    "https://www.saashub.com/uptimepage-dev",
    "https://stackshare.io/uptimepage",
    "https://alternativeto.net/software/uptimepage/",
    "https://www.nxgntools.com/tools/uptimepage",
    "https://ufind.best/products/uptimepage",
];

/// Prose overview for `llms.txt` / `llms-full.txt` — what the product is,
/// in the words an assistant should reach for when asked about it.
const LLMS_OVERVIEW: &str = "Uptimepage is a hosted uptime-monitoring and public status-page service for teams. \
Checks run as often as every 60 seconds from five regions (San Jose, New York, Frankfurt, Helsinki and Singapore); a failing check opens an incident automatically and posts it to a branded \
status page on your own subdomain. Alerts carry dedupe and flap-suppression so brief blips never page on-call. \
Organizations have role-based members and an audit log, and monitors, status pages and incidents are managed by \
REST API, Terraform or MCP. The production source is published under AGPL, so a team can audit what it runs and \
self-host if its requirements change. Public data is available as JSON, an RSS feed and an embeddable SVG badge. \
Uptimepage operates the hosted service at uptimepage.dev: the Standard plan is free with no card, the first 1,000 \
accounts get a more generous founding plan kept for life, and Pro is paid for teams in production.";

/// Machine-readable product facts. Authored single source for the
/// llms files — keep terse, factual, and current.
const LLMS_FACTS: &[(&str, &str)] = &[
    (
        "Check types",
        "HTTP/HTTPS, TCP, DNS, TLS certificate, domain expiry, ICMP ping, cron-job heartbeat, scripted browser login flow",
    ),
    ("Check interval", "every 60 seconds"),
    (
        "Check regions",
        "5 hosted regions: San Jose, New York, Frankfurt, Helsinki, Singapore; self-hosted can add any region by running a probe agent",
    ),
    (
        "Alert channels",
        "Slack, Discord, Telegram, Microsoft Teams, Google Chat, Mattermost, email, SMS, webhook, PagerDuty, ntfy, Pushover, Gotify, WhatsApp",
    ),
    (
        "Status page",
        "branded (logo + colour) on your own subdomain, or your own domain on Pro and Team",
    ),
    ("Public history", "90 days"),
    (
        "Incidents",
        "auto-opened on down, auto-closed on recovery, with public notes",
    ),
    (
        "Scheduled maintenance",
        "windows that silence the page engine",
    ),
    ("Data export", "JSON API, RSS feed, embeddable SVG badge"),
    ("Terraform provider", TERRAFORM_URL),
    (
        "Team",
        "role-based members, GitHub or email invites, audit log",
    ),
    (
        "Pricing",
        "Standard is free with no card. Founding is free for the first 1,000 accounts and kept for life. Pro is $9/month or $90/year and Team is $19/month or $190/year; a downgrade or cancel takes effect at the end of the paid period. Self-host is free under AGPL.",
    ),
    (
        "Plan limits",
        "Standard: 20 monitors, checks every 3 minutes, 30-day history, 3 of 5 global regions, 1 status page with 15 components, 3 team members, every alert channel, API and MCP. Founding adds 50 monitors including 1 browser login flow, 60-second checks, all regions, 90-day history, 5 team members, 2 status pages and BYO SMS, free for the first 1,000 accounts and kept for life. Pro is $9/month: the founding limits plus 3 browser login flows, a custom status-page domain and white-label. Team is $19/month and adds 150 monitors including 10 browser login flows, 30-second checks, 13-month history, 15 team members, 5 status pages, and on-call rotations with escalation policies.",
    ),
    (
        "Deployment",
        "hosted service at uptimepage.dev is the primary product",
    ),
    (
        "Self-hosting",
        "AGPL, run it yourself with docker compose (Postgres + ClickHouse), unlimited monitors on your own hardware",
    ),
    ("Source code", SOURCE_URL),
    ("License", "AGPL-3.0"),
    (
        "Sign-in",
        "GitHub, Google, passkey, or an emailed link and code",
    ),
];

const DEFAULT_OG_CARD: &str = "/static/marketing/og.png";

#[derive(Debug, Clone, Serialize)]
pub struct OpenGraph {
    pub title: String,
    pub description: String,
    pub og_type: String,
    pub url: String,
    pub image: String,
    pub image_alt: String,
}

impl OpenGraph {
    pub fn default_for(title: &str, url: &str, canonical_origin: &str) -> Self {
        Self {
            title: title.to_string(),
            description: META_DESCRIPTION.to_string(),
            og_type: "website".to_string(),
            url: url.to_string(),
            image: absolute_asset(canonical_origin, DEFAULT_OG_CARD),
            image_alt: title.to_string(),
        }
    }

    pub fn for_post(
        canonical_origin: &str,
        title: &str,
        excerpt: &str,
        slug: &str,
        og_image: Option<&str>,
    ) -> Self {
        Self {
            title: title.to_string(),
            description: excerpt.to_string(),
            og_type: "article".to_string(),
            url: format!("{canonical_origin}/blog/{slug}"),
            image: absolute_asset(canonical_origin, og_image.unwrap_or(DEFAULT_OG_CARD)),
            image_alt: title.to_string(),
        }
    }
}

/// JSON-LD blob rendered into a `<script type="application/ld+json">`.
/// Stored as the serialised string so the template emits it verbatim
/// through `|safe`.
#[derive(Debug, Clone)]
pub struct JsonLd(String);

impl JsonLd {
    /// Serialize for a `<script>` block, escaping the characters `serde_json`
    /// leaves raw (`<`, `>`, `&`) that would otherwise let a string value
    /// close the element early. Every emitter builds through here, so the
    /// template's `|safe` can't emit an unescaped payload.
    fn from_value(value: serde_json::Value) -> Self {
        let escaped = value
            .to_string()
            .replace('<', "\\u003c")
            .replace('>', "\\u003e")
            .replace('&', "\\u0026")
            .replace('\u{2028}', "\\u2028")
            .replace('\u{2029}', "\\u2029");
        JsonLd(escaped)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

pub fn json_ld_organization(canonical_origin: &str) -> JsonLd {
    let payload = serde_json::json!({
        "@context": "https://schema.org",
        "@type": "Organization",
        "@id": format!("{canonical_origin}/#organization"),
        "name": BRAND,
        "url": canonical_origin,
        "logo": absolute_asset(canonical_origin, "/static/img/favicon-512.png"),
        "email": CONTACT_EMAIL,
        "address": {
            "@type": "PostalAddress",
            "addressLocality": ORG_LOCALITY,
            "addressCountry": ORG_COUNTRY,
        },
        "foundingDate": ORG_FOUNDING_DATE,
        "founder": founder(canonical_origin),
        "sameAs": ORG_SAME_AS,
    });
    JsonLd::from_value(payload)
}

/// A reference node: the full Person lives on the about page under the same
/// `@id`, so the organization links to one entity instead of restating it.
fn founder(canonical_origin: &str) -> serde_json::Value {
    serde_json::json!({
        "@type": "Person",
        "@id": author_id(canonical_origin),
        "name": AUTHOR.name,
        "url": format!("{canonical_origin}{AUTHOR_PAGE}"),
    })
}

/// Canonical product entity (`@id`), with a freemium `AggregateOffer` so
/// search and answer engines read the real price range, not a guessed tier.
///
/// `price` sits alongside `lowPrice`/`highPrice` because Google requires
/// `offers.price` for the Software App result and accepts no range in its
/// place; 0 is what it prescribes for an app usable without payment.
pub fn json_ld_software_application(canonical_origin: &str) -> JsonLd {
    let payload = serde_json::json!({
        "@context": "https://schema.org",
        "@type": "SoftwareApplication",
        "@id": format!("{canonical_origin}/#software"),
        "name": BRAND,
        "applicationCategory": "DeveloperApplication",
        "operatingSystem": "Web, Docker, Linux",
        "url": canonical_origin,
        "isAccessibleForFree": true,
        // ImageObject over a bare URL so each shot carries its caption.
        "screenshot": gallery::SHOTS
            .iter()
            .map(|s| serde_json::json!({
                "@type": "ImageObject",
                "contentUrl": gallery::absolute_url(canonical_origin, s),
                "caption": s.caption,
                "width": s.width,
                "height": s.height,
            }))
            .collect::<Vec<_>>(),
        "offers": {
            "@type": "AggregateOffer",
            "price": "0",
            "lowPrice": "0",
            "highPrice": "19",
            "priceCurrency": "USD",
            "offerCount": "4",
        },
        "publisher": { "@id": format!("{canonical_origin}/#organization") },
    });
    JsonLd::from_value(payload)
}

/// `SoftwareSourceCode` for the AGPL, self-hostable side of the product. A
/// rating-free entity that answers "open source / self-hosted" queries and
/// links back to the product and organization entities.
pub fn json_ld_software_source_code(canonical_origin: &str) -> JsonLd {
    let payload = serde_json::json!({
        "@context": "https://schema.org",
        "@type": "SoftwareSourceCode",
        "name": BRAND,
        "url": canonical_origin,
        "codeRepository": "https://github.com/uptimepage/uptimepage",
        "programmingLanguage": "Rust",
        "license": "https://www.gnu.org/licenses/agpl-3.0.html",
        "runtimePlatform": "Docker",
        "about": { "@id": format!("{canonical_origin}/#software") },
        "author": { "@id": format!("{canonical_origin}/#organization") },
    });
    JsonLd::from_value(payload)
}

/// `WebApplication` for a standalone free tool: its own entity, marked free,
/// published by the org. Distinct from `json_ld_software_application`, which
/// describes the product itself.
pub fn json_ld_web_application(
    canonical_origin: &str,
    name: &str,
    path: &str,
    description: &str,
) -> JsonLd {
    let payload = serde_json::json!({
        "@context": "https://schema.org",
        "@type": "WebApplication",
        "name": name,
        "url": format!("{canonical_origin}{path}"),
        "description": description,
        "applicationCategory": "DeveloperApplication",
        "operatingSystem": "Web",
        "isAccessibleForFree": true,
        "offers": { "@type": "Offer", "price": "0", "priceCurrency": "USD" },
        "publisher": { "@id": format!("{canonical_origin}/#organization") },
    });
    JsonLd::from_value(payload)
}

/// Schema text must match the visible FAQ, so this builds from the same pairs.
pub fn json_ld_faqpage(faqs: &[(&str, &str)]) -> JsonLd {
    let main_entity: Vec<_> = faqs
        .iter()
        .map(|(q, a)| {
            serde_json::json!({
                "@type": "Question",
                "name": q,
                "acceptedAnswer": { "@type": "Answer", "text": a },
            })
        })
        .collect();
    let payload = serde_json::json!({
        "@context": "https://schema.org",
        "@type": "FAQPage",
        "mainEntity": main_entity,
    });
    JsonLd::from_value(payload)
}

/// `BreadcrumbList` for a second-level marketing page (Home › page). Gives
/// search engines an explicit Home → page trail for the listing.
pub fn json_ld_breadcrumb(canonical_origin: &str, name: &str, path: &str) -> JsonLd {
    let payload = serde_json::json!({
        "@context": "https://schema.org",
        "@type": "BreadcrumbList",
        "itemListElement": [
            { "@type": "ListItem", "position": 1, "name": "Home", "item": canonical_origin },
            { "@type": "ListItem", "position": 2, "name": name, "item": format!("{canonical_origin}{path}") },
        ],
    });
    JsonLd::from_value(payload)
}

/// `BreadcrumbList` for a page nested deeper than one level (Home › Docs ›
/// page). `trail` is every step after Home, in order.
pub fn json_ld_breadcrumb_trail(canonical_origin: &str, trail: &[(&str, &str)]) -> JsonLd {
    let mut items = vec![serde_json::json!({
        "@type": "ListItem", "position": 1, "name": "Home", "item": canonical_origin,
    })];
    for (i, (name, path)) in trail.iter().enumerate() {
        items.push(serde_json::json!({
            "@type": "ListItem",
            "position": i + 2,
            "name": name,
            "item": format!("{canonical_origin}{path}"),
        }));
    }
    JsonLd::from_value(serde_json::json!({
        "@context": "https://schema.org",
        "@type": "BreadcrumbList",
        "itemListElement": items,
    }))
}

pub fn json_ld_webpage(
    canonical_origin: &str,
    path: &str,
    name: &str,
    created: &str,
    modified: &str,
    about_product: bool,
) -> JsonLd {
    let mut payload = serde_json::json!({
        "@context": "https://schema.org",
        "@type": "WebPage",
        "@id": format!("{canonical_origin}{path}#webpage"),
        "name": name,
        "url": format!("{canonical_origin}{path}"),
        "datePublished": iso_datetime(created),
        "dateModified": iso_datetime(modified),
        "isPartOf": { "@id": format!("{canonical_origin}/#website") },
        "publisher": { "@id": format!("{canonical_origin}/#organization") },
    });
    // Only pages whose subject is the product link `about` to it; a rival-vs-rival
    // comparison page would misdescribe itself by claiming to be about us.
    if about_product {
        payload["about"] = serde_json::json!({ "@id": format!("{canonical_origin}/#software") });
    }
    JsonLd::from_value(payload)
}

/// `TechArticle` for a documentation page. Docs are reference material
/// about the product, not editorial, so they carry the technical type
/// rather than `BlogPosting`.
pub fn json_ld_tech_article(
    canonical_origin: &str,
    path: &str,
    name: &str,
    description: &str,
    modified: &str,
) -> JsonLd {
    let payload = serde_json::json!({
        "@context": "https://schema.org",
        "@type": "TechArticle",
        "@id": format!("{canonical_origin}{path}#article"),
        "headline": name,
        "name": name,
        "description": description,
        "url": format!("{canonical_origin}{path}"),
        "dateModified": iso_datetime(modified),
        "inLanguage": "en",
        "isPartOf": { "@id": format!("{canonical_origin}/#website") },
        "about": { "@id": format!("{canonical_origin}/#software") },
        "author": author(canonical_origin),
        "publisher": { "@id": format!("{canonical_origin}/#organization") },
    });
    JsonLd::from_value(payload)
}

/// `Article` for a changelog entry: dated first-party prose about the
/// product, so it is neither a `BlogPosting` (editorial) nor a `TechArticle`
/// (reference).
pub fn json_ld_article(
    canonical_origin: &str,
    path: &str,
    headline: &str,
    description: &str,
    date: &str,
) -> JsonLd {
    let payload = serde_json::json!({
        "@context": "https://schema.org",
        "@type": "Article",
        "@id": format!("{canonical_origin}{path}#article"),
        "headline": headline,
        "description": description,
        "url": format!("{canonical_origin}{path}"),
        "mainEntityOfPage": format!("{canonical_origin}{path}"),
        "datePublished": iso_datetime(date),
        "dateModified": iso_datetime(date),
        "inLanguage": "en",
        "isPartOf": { "@id": format!("{canonical_origin}/#website") },
        "about": { "@id": format!("{canonical_origin}/#software") },
        "author": author(canonical_origin),
        "publisher": { "@id": format!("{canonical_origin}/#organization") },
    });
    JsonLd::from_value(payload)
}

pub fn json_ld_website(canonical_origin: &str) -> JsonLd {
    let payload = serde_json::json!({
        "@context": "https://schema.org",
        "@type": "WebSite",
        "@id": format!("{canonical_origin}/#website"),
        "name": BRAND,
        "url": canonical_origin,
        "publisher": { "@id": format!("{canonical_origin}/#organization") },
    });
    JsonLd::from_value(payload)
}

/// `ItemList` for list-format posts ("best X tools"): the ranked items a
/// crawler reads off the article. Names only; the list lives on this URL.
pub fn json_ld_item_list(
    canonical_origin: &str,
    slug: &str,
    name: &str,
    items: &[String],
) -> JsonLd {
    let entries: Vec<_> = items
        .iter()
        .enumerate()
        .map(|(i, item)| {
            serde_json::json!({
                "@type": "ListItem",
                "position": i + 1,
                "name": item,
                "item": { "@type": "Thing", "name": item },
            })
        })
        .collect();
    let payload = serde_json::json!({
        "@context": "https://schema.org",
        "@type": "ItemList",
        "name": name,
        "url": format!("{canonical_origin}/blog/{slug}"),
        "itemListElement": entries,
    });
    JsonLd::from_value(payload)
}

/// ItemList whose entries link to real pages, for a hub that lists other
/// pages (the tools index). Each item carries its own URL so search and LLM
/// consumers can follow it, unlike [`json_ld_item_list`] which lists bare names.
pub fn json_ld_item_list_links(
    canonical_origin: &str,
    page_path: &str,
    name: &str,
    items: &[(&str, &str)],
) -> JsonLd {
    let entries: Vec<_> = items
        .iter()
        .enumerate()
        .map(|(i, (item_name, item_path))| {
            serde_json::json!({
                "@type": "ListItem",
                "position": i + 1,
                "name": item_name,
                "url": format!("{canonical_origin}{item_path}"),
            })
        })
        .collect();
    let payload = serde_json::json!({
        "@context": "https://schema.org",
        "@type": "ItemList",
        "name": name,
        "url": format!("{canonical_origin}{page_path}"),
        "itemListElement": entries,
    });
    JsonLd::from_value(payload)
}

pub fn json_ld_blog_posting(
    canonical_origin: &str,
    title: &str,
    excerpt: &str,
    slug: &str,
    date_published: &str,
    date_modified: &str,
    image: &str,
) -> JsonLd {
    let url = format!("{canonical_origin}/blog/{slug}");
    let payload = serde_json::json!({
        "@context": "https://schema.org",
        "@type": "BlogPosting",
        "headline": title,
        "description": excerpt,
        "image": image,
        "datePublished": iso_datetime(date_published),
        "dateModified": iso_datetime(date_modified),
        "mainEntityOfPage": url,
        "author": author(canonical_origin),
        "publisher": publisher(canonical_origin),
    });
    JsonLd::from_value(payload)
}

/// `Blog` listing with its posts as `blogPost` entries — the item list a
/// crawler reads off `/blog`. Pair with `json_ld_breadcrumb` on the page.
pub fn json_ld_blog(canonical_origin: &str, posts: &[(&str, &str, &str)]) -> JsonLd {
    let entries: Vec<_> = posts
        .iter()
        .map(|(title, slug, date)| {
            serde_json::json!({
                "@type": "BlogPosting",
                "headline": title,
                "url": format!("{canonical_origin}/blog/{slug}"),
                "datePublished": iso_datetime(date),
                "author": { "@id": author_id(canonical_origin) },
            })
        })
        .collect();
    let payload = serde_json::json!({
        "@context": "https://schema.org",
        "@type": "Blog",
        "name": format!("{BRAND} Blog"),
        "url": format!("{canonical_origin}/blog"),
        "author": author(canonical_origin),
        "publisher": publisher(canonical_origin),
        "blogPost": entries,
    });
    JsonLd::from_value(payload)
}

/// Landing that carries the author's `Person` node; the `@id` fragment
/// resolves here rather than to a page that never describes the entity.
pub const AUTHOR_PAGE: &str = "/about";

fn author_id(canonical_origin: &str) -> String {
    format!("{canonical_origin}{AUTHOR_PAGE}#author")
}

/// One `@id`-addressable Person so every post resolves to the same entity
/// instead of a fresh look-alike node per page. `url` is the on-site page
/// describing the author; the third-party profiles stay in `sameAs`.
fn author(canonical_origin: &str) -> serde_json::Value {
    let same_as: Vec<&str> = AUTHOR.same_as.iter().map(|(_, u)| *u).collect();
    serde_json::json!({
        "@type": "Person",
        "@id": author_id(canonical_origin),
        "name": AUTHOR.name,
        "url": format!("{canonical_origin}{AUTHOR_PAGE}"),
        "jobTitle": AUTHOR.role,
        "description": AUTHOR.bio,
        "image": absolute_asset(canonical_origin, &format!("/static/{}", AUTHOR.image)),
        "sameAs": same_as,
    })
}

/// Standalone `Person`, for the page the author `@id` resolves to.
pub fn json_ld_person(canonical_origin: &str) -> JsonLd {
    let mut payload = author(canonical_origin);
    payload["@context"] = serde_json::json!("https://schema.org");
    JsonLd::from_value(payload)
}

/// Logo is a raster: Google's logo guidance needs pixel dimensions an SVG lacks.
fn publisher(canonical_origin: &str) -> serde_json::Value {
    serde_json::json!({
        "@type": "Organization",
        "@id": format!("{canonical_origin}/#organization"),
        "name": BRAND,
        "url": canonical_origin,
        "logo": {
            "@type": "ImageObject",
            "url": absolute_asset(canonical_origin, "/static/img/favicon-512.png"),
            "width": 512,
            "height": 512,
        },
    })
}

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

/// Curated index for assistants — the `llms.txt` convention: title,
/// one-line summary, prose overview, then link sections. Built from the
/// same tables that drive the router and sitemap, so it never drifts.
fn build_llms(cfg: &MarketingCfg) -> Bytes {
    let origin = &cfg.canonical_origin;
    let mut s = String::new();
    s.push_str(&format!("# {BRAND}\n\n> {TAGLINE}\n\n{LLMS_OVERVIEW}\n\n"));

    s.push_str("## Product\n");
    s.push_str(&format!(
        "- [Homepage]({origin}): Product overview, features and pricing.\n"
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
        "- [Terraform provider]({TERRAFORM_URL}): Manage monitors, status pages and notification channels as config-as-code.\n\n"
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

    s.push_str("## Facts\n");
    for (k, v) in LLMS_FACTS {
        s.push_str(&format!("- {k}: {v}\n"));
    }
    if let Some(mcp) = cfg.mcp_url.as_deref() {
        s.push_str(&format!("- MCP server: {mcp}\n"));
    }
    s.push('\n');

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
        SitemapUrl::new(format!("{origin}/pricing"), None),
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

fn absolute_asset(canonical_origin: &str, path: &str) -> String {
    format!("{canonical_origin}{path}")
}

/// Bare `YYYY-MM-DD` dates carry no zone; widen them to the full ISO 8601
/// instant so a crawler reads one timestamp rather than guessing a zone.
fn iso_datetime(date: &str) -> Cow<'_, str> {
    let bare = date.len() == 10
        && date.bytes().enumerate().all(|(i, b)| match i {
            4 | 7 => b == b'-',
            _ => b.is_ascii_digit(),
        });
    if bare {
        Cow::Owned(format!("{date}T00:00:00+00:00"))
    } else {
        Cow::Borrowed(date)
    }
}

pub(crate) fn xml_escape(s: &str) -> Cow<'_, str> {
    if !s
        .bytes()
        .any(|b| matches!(b, b'&' | b'<' | b'>' | b'"' | b'\''))
    {
        return Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            other => out.push(other),
        }
    }
    Cow::Owned(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn og_image_is_absolute_https() {
        let og = OpenGraph::default_for(
            "Hi",
            "https://uptimepage.dev/pricing",
            "https://uptimepage.dev",
        );
        // og:url is the page; og:image is rooted at the origin, not the page.
        assert_eq!(og.url, "https://uptimepage.dev/pricing");
        assert_eq!(og.image, "https://uptimepage.dev/static/marketing/og.png");
    }

    #[test]
    fn json_ld_renders_valid_json() {
        let jl = json_ld_organization("https://uptimepage.dev");
        let v: serde_json::Value = serde_json::from_str(jl.as_str()).unwrap();
        assert_eq!(v["@type"], "Organization");
        assert_eq!(v["url"], "https://uptimepage.dev");
    }

    /// Reaches for /about too: the JSON-LD is invisible on the page, so it
    /// goes stale unnoticed when the contact rows change.
    #[test]
    fn the_organization_node_carries_the_contact_facts() {
        let jl = json_ld_organization("https://uptimepage.dev");
        let v: serde_json::Value = serde_json::from_str(jl.as_str()).unwrap();
        assert_eq!(v["email"], CONTACT_EMAIL);
        assert_eq!(v["address"]["@type"], "PostalAddress");
        assert_eq!(v["address"]["addressLocality"], ORG_LOCALITY);
        assert_eq!(v["address"]["addressCountry"], ORG_COUNTRY);
        assert_eq!(v["foundingDate"], ORG_FOUNDING_DATE);
        assert_eq!(v["founder"]["@id"], author_id("https://uptimepage.dev"));
        assert_eq!(v["founder"]["name"], AUTHOR.name);
        let about = landings::LANDINGS
            .iter()
            .find(|l| l.path == "/about")
            .expect("/about is a landing");
        assert!(
            about
                .features
                .iter()
                .any(|f| f.value.contains(ORG_LOCALITY)),
            "/about no longer says where the company is; the JSON-LD still claims {ORG_LOCALITY}"
        );
        assert!(
            about.features.iter().any(|f| f.value == CONTACT_EMAIL),
            "/about no longer lists {CONTACT_EMAIL}; the JSON-LD still claims it"
        );
    }

    #[test]
    fn json_ld_item_list_orders_positions() {
        let items = vec!["Uptime Kuma".to_string(), "Gatus".to_string()];
        let jl = json_ld_item_list("https://uptimepage.dev", "best-tools", "Best tools", &items);
        let v: serde_json::Value = serde_json::from_str(jl.as_str()).unwrap();
        assert_eq!(v["@type"], "ItemList");
        assert_eq!(v["url"], "https://uptimepage.dev/blog/best-tools");
        assert_eq!(v["itemListElement"][0]["position"], 1);
        assert_eq!(v["itemListElement"][1]["name"], "Gatus");
    }

    #[test]
    fn software_application_carries_every_screenshot() {
        let jl = json_ld_software_application("https://uptimepage.dev");
        let v: serde_json::Value = serde_json::from_str(jl.as_str()).unwrap();
        let shots = v["screenshot"].as_array().expect("screenshot is an array");
        assert_eq!(shots.len(), gallery::SHOTS.len());
        assert_eq!(shots[0]["@type"], "ImageObject");
        assert!(
            shots[0]["contentUrl"]
                .as_str()
                .unwrap()
                .starts_with("https://uptimepage.dev/static/marketing/"),
            "contentUrl must be absolute and origin-rooted: {:?}",
            shots[0]["contentUrl"]
        );
        for (shot, json) in gallery::SHOTS.iter().zip(shots) {
            assert_eq!(json["caption"], shot.caption);
        }
    }

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

    #[test]
    fn json_ld_software_application_is_free() {
        let jl = json_ld_software_application("https://uptimepage.dev");
        let v: serde_json::Value = serde_json::from_str(jl.as_str()).unwrap();
        assert_eq!(v["@type"], "SoftwareApplication");
        assert_eq!(v["@id"], "https://uptimepage.dev/#software");
        assert_eq!(v["isAccessibleForFree"], true);
        assert_eq!(v["offers"]["@type"], "AggregateOffer");
        assert_eq!(v["offers"]["lowPrice"], "0");
        assert_eq!(v["offers"]["priceCurrency"], "USD");
        // Google requires offers.price for Software App and takes no range in
        // its place, so dropping this silently forfeits the result.
        assert_eq!(v["offers"]["price"], "0");
        assert_eq!(
            v["publisher"]["@id"],
            "https://uptimepage.dev/#organization"
        );
    }

    #[test]
    fn json_ld_item_list_nests_item_for_carousel() {
        let items = vec!["Uptime Kuma".to_string()];
        let jl = json_ld_item_list("https://uptimepage.dev", "best-tools", "Best tools", &items);
        let v: serde_json::Value = serde_json::from_str(jl.as_str()).unwrap();
        assert_eq!(v["itemListElement"][0]["item"]["@type"], "Thing");
        assert_eq!(v["itemListElement"][0]["item"]["name"], "Uptime Kuma");
    }

    #[test]
    fn json_ld_breadcrumb_trail_numbers_every_level_from_home() {
        let jl = json_ld_breadcrumb_trail(
            "https://uptimepage.dev",
            &[("Docs", "/docs"), ("Probe regions", "/docs/hosted/regions")],
        );
        let v: serde_json::Value = serde_json::from_str(jl.as_str()).unwrap();
        let items = v["itemListElement"].as_array().unwrap();
        let steps: Vec<(u64, &str)> = items
            .iter()
            .map(|i| (i["position"].as_u64().unwrap(), i["name"].as_str().unwrap()))
            .collect();
        assert_eq!(steps, [(1, "Home"), (2, "Docs"), (3, "Probe regions")]);
        assert_eq!(
            items[2]["item"],
            "https://uptimepage.dev/docs/hosted/regions"
        );
    }

    #[test]
    fn json_ld_webpage_links_product_and_site() {
        let jl = json_ld_webpage(
            "https://uptimepage.dev",
            "/pricing",
            "Pricing",
            "2026-01-01",
            "2026-01-02",
            true,
        );
        let v: serde_json::Value = serde_json::from_str(jl.as_str()).unwrap();
        assert_eq!(v["about"]["@id"], "https://uptimepage.dev/#software");
        assert_eq!(v["isPartOf"]["@id"], "https://uptimepage.dev/#website");
    }

    #[test]
    fn json_ld_webpage_omits_about_for_comparison() {
        let jl = json_ld_webpage(
            "https://uptimepage.dev",
            "/compare/a-vs-b",
            "A vs B",
            "2026-01-01",
            "2026-01-02",
            false,
        );
        let v: serde_json::Value = serde_json::from_str(jl.as_str()).unwrap();
        assert!(v["about"].is_null());
        assert_eq!(v["isPartOf"]["@id"], "https://uptimepage.dev/#website");
    }

    #[test]
    fn json_ld_source_code_is_agpl() {
        let jl = json_ld_software_source_code("https://uptimepage.dev");
        let v: serde_json::Value = serde_json::from_str(jl.as_str()).unwrap();
        assert_eq!(v["@type"], "SoftwareSourceCode");
        assert_eq!(
            v["codeRepository"],
            "https://github.com/uptimepage/uptimepage"
        );
        assert_eq!(v["about"]["@id"], "https://uptimepage.dev/#software");
    }

    #[test]
    fn json_ld_faqpage_carries_questions() {
        let jl = json_ld_faqpage(&[("Q1?", "A1"), ("Q2?", "A2")]);
        let v: serde_json::Value = serde_json::from_str(jl.as_str()).unwrap();
        assert_eq!(v["@type"], "FAQPage");
        assert_eq!(v["mainEntity"].as_array().unwrap().len(), 2);
        assert_eq!(v["mainEntity"][0]["acceptedAnswer"]["text"], "A1");
    }

    #[test]
    fn json_ld_blog_posting_dates_carry_a_zone() {
        let jl = json_ld_blog_posting(
            "https://x.test",
            "T",
            "E",
            "s",
            "2026-06-20",
            "2026-07-15T09:30:00+02:00",
            "https://x.test/og.png",
        );
        let v: serde_json::Value = serde_json::from_str(jl.as_str()).unwrap();
        assert_eq!(v["datePublished"], "2026-06-20T00:00:00+00:00");
        assert_eq!(v["dateModified"], "2026-07-15T09:30:00+02:00");
    }

    #[test]
    fn json_ld_blog_posts_share_one_author_node() {
        let post = json_ld_blog_posting(
            "https://x.test",
            "T",
            "E",
            "s",
            "2026-06-20",
            "2026-06-20",
            "https://x.test/og.png",
        );
        let post: serde_json::Value = serde_json::from_str(post.as_str()).unwrap();
        assert_eq!(post["author"]["@id"], "https://x.test/about#author");
        assert_eq!(post["author"]["name"], AUTHOR.name);

        let list = json_ld_blog("https://x.test", &[("T", "s", "2026-06-20")]);
        let list: serde_json::Value = serde_json::from_str(list.as_str()).unwrap();
        assert_eq!(list["author"]["@id"], "https://x.test/about#author");
        assert_eq!(
            list["blogPost"][0]["author"],
            serde_json::json!({ "@id": "https://x.test/about#author" }),
            "listing entries reference the node, not a duplicate of it"
        );
    }

    #[test]
    fn json_ld_person_points_on_site_and_keeps_profiles_in_same_as() {
        let jl = json_ld_person("https://x.test");
        let v: serde_json::Value = serde_json::from_str(jl.as_str()).unwrap();
        assert_eq!(v["@context"], "https://schema.org");
        assert_eq!(v["@type"], "Person");
        assert_eq!(v["@id"], "https://x.test/about#author");
        assert_eq!(v["url"], "https://x.test/about");
        let same_as = v["sameAs"].as_array().unwrap();
        assert!(
            same_as.iter().any(|u| u == AUTHOR.url),
            "the off-site profile belongs in sameAs: {same_as:?}"
        );
    }

    #[test]
    fn json_ld_escapes_script_breakout() {
        let ld = json_ld_blog(
            "https://x.test",
            &[("</script><script>alert(1)</script>", "s", "2026-01-01")],
        );
        let out = ld.as_str();
        assert!(!out.contains("</script>"), "raw </script> leaked: {out}");
        assert!(out.contains("\\u003c"), "expected escaped <, got: {out}");
        let v: serde_json::Value = serde_json::from_str(out).expect("still valid JSON");
        assert_eq!(v["@type"], "Blog");
        assert_eq!(
            v["blogPost"][0]["headline"],
            "</script><script>alert(1)</script>"
        );
    }

    #[test]
    fn xml_escape_handles_ampersand() {
        assert_eq!(xml_escape("a&b<c>\"d"), "a&amp;b&lt;c&gt;&quot;d");
    }

    #[test]
    fn xml_escape_borrows_when_clean() {
        let s = "no-escapes-needed";
        assert!(matches!(xml_escape(s), Cow::Borrowed(_)));
    }
}
