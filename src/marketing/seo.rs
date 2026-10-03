//! Per-page SEO primitives: OpenGraph and JSON-LD payloads, and the product
//! facts they share with the crawler files in `site_files`.
//!
//! Absolute URLs are mandatory in `og:image`, `og:url`, and `<link
//! rel="canonical">` — social-card scrapers reject relative paths
//! silently — so every URL emitted here is prefixed with
//! `MarketingCfg::canonical_origin`.

use std::borrow::Cow;

use serde::Serialize;

use super::config::{
    AUTHOR, BRAND, CONTACT_EMAIL, META_DESCRIPTION, ORG_COUNTRY, ORG_FOUNDING_DATE, ORG_LOCALITY,
    SOURCE_URL, TERRAFORM_URL,
};
use super::gallery;

/// Public profiles that establish the brand entity for search engines.
const ORG_SAME_AS: &[&str] = &[
    "https://github.com/uptimepage",
    SOURCE_URL,
    "https://x.com/uptimepageHQ",
    "https://bsky.app/profile/uptimepage.bsky.social",
    "https://www.saashub.com/uptimepage-dev",
    "https://stackshare.io/uptimepage",
    "https://alternativeto.net/software/uptimepage/",
    "https://www.nxgntools.com/tools/uptimepage",
    "https://ufind.best/products/uptimepage",
];

/// Machine-readable product facts. Authored single source for the
/// llms files — keep terse, factual, and current.
pub(super) const LLMS_FACTS: &[(&str, &str)] = &[
    (
        "Check types",
        "HTTP/HTTPS, TCP, DNS, TLS certificate, domain expiry, ICMP ping, cron-job heartbeat, scripted browser login flow, manual (state set by an operator)",
    ),
    (
        "Check interval",
        "Standard every 3 minutes, Founding and Pro every 60 seconds, Team every 30 seconds. Self-hosted installs start on the Team plan, and the operator can lower it to 10 seconds for HTTP, TCP, DNS and ping. Browser flows, TLS and domain checks have higher floors",
    ),
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
        "auto-opened on down, auto-closed on recovery, acknowledged by a responder, with public notes",
    ),
    (
        "On-call schedules",
        "layered rotations (daily, weekly or a custom length) in the schedule's timezone, optional weekly hours per layer, calendar overrides for holidays and swaps, each person's shifts as an iCalendar feed",
    ),
    (
        "Escalation policies",
        "ordered levels that page notification channels and on-call schedules, a wait between levels, up to 10 repeats of the ladder, stopped by acknowledging or resolving; bound per monitor or set as the org default",
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

/// The `LLMS_FACTS` rows that make up `SoftwareApplication.featureList`, so an
/// answer engine reads monitoring, status pages and on-call as one application.
const FEATURE_FACTS: &[&str] = &[
    "Check types",
    "Check regions",
    "Alert channels",
    "Status page",
    "Incidents",
    "On-call schedules",
    "Escalation policies",
];

fn feature_list() -> Vec<String> {
    LLMS_FACTS
        .iter()
        .filter(|(k, _)| FEATURE_FACTS.contains(k))
        .map(|(k, v)| format!("{k}: {v}"))
        .collect()
}

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
        "featureList": feature_list(),
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
    created: &str,
    modified: &str,
    image: &str,
) -> JsonLd {
    let payload = serde_json::json!({
        "@context": "https://schema.org",
        "@type": "TechArticle",
        "@id": format!("{canonical_origin}{path}#article"),
        "headline": name,
        "name": name,
        "description": description,
        "url": format!("{canonical_origin}{path}"),
        "image": image,
        "datePublished": iso_datetime(created),
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
        assert_eq!(
            v["featureList"].as_array().unwrap().len(),
            FEATURE_FACTS.len(),
            "a FEATURE_FACTS key no longer names an LLMS_FACTS row"
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
