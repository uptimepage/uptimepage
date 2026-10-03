//! The home page: rendered once into a `OnceLock` at boot, so every request
//! after that serves the cached body and a stable ETag with no askama work.
//! Its Markdown representation is the `llms.txt` site index.

use std::sync::{Arc, OnceLock};

use askama::Template;
use askama_web::WebTemplate;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::Response;

use crate::templates::filters;

use super::config::{BRAND, MarketingCfg};
use super::gallery;
use super::pages::{CachedRender, PAGE_CACHE_CONTROL, body_etag, cached_render};
use super::seo::{
    JsonLd, OpenGraph, json_ld_faqpage, json_ld_organization, json_ld_software_application,
    json_ld_software_source_code, json_ld_website,
};
use super::{negotiate, site_files};

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
         out of the box. On Pro and Team it can also live on your own hostname, such as \
         <code class=\"mk-chip\" translate=\"no\">status.yourcompany.com</code>. Setup is by email for now: \
         send the hostname, add the CNAME I reply with, and the certificate is issued automatically.",
    ),
    (
        "What kinds of monitors are supported?",
        "HTTP/HTTPS, TCP port, DNS lookup, ICMP ping, cron-job heartbeat, TLS-certificate \
         and domain expiry, and a browser login flow that signs in for real and charts how \
         long each step takes, plus manual monitors whose state you set by hand. Per-monitor headers, basic-auth, bearer tokens, expected \
         status code, content-match, TLS verification, follow-redirects rules.",
    ),
    (
        "Where do alerts come from?",
        "Slack, Discord, Teams, Google Chat, Mattermost, Telegram, email, SMS, PagerDuty, ntfy, \
         Pushover, Gotify, WhatsApp, or any HTTPS webhook. Each monitor can carry its own \
         escalation policy, so a marketing-site flap can page a quiet channel while your API’s monitor pages on-call.",
    ),
    (
        "Can it page whoever is on call?",
        "Yes. On-call schedules rotate your team daily, weekly or on a length you set, \
         with overrides for holidays and swaps, and each person can add their shifts to \
         their calendar app. Escalation policies page the next level when nobody \
         acknowledges, and acknowledging or resolving the incident stops them.",
    ),
    (
        "Can I export my data?",
        "Always. JSON export per monitor and incident. RSS for public incidents. \
         SVG badges you can drop in a README.",
    ),
];

/// One landing render per process. The body is invariant after boot —
/// `app_url`, `canonical_origin`, version, JSON-LD all come from
/// startup config — so re-rendering and re-hashing per request would
/// burn ~80–150 µs for an identical response.
static LANDING_CACHED: OnceLock<CachedRender> = OnceLock::new();
static LANDING_MD: OnceLock<CachedRender> = OnceLock::new();

fn render_landing(cfg: &MarketingCfg) -> CachedRender {
    let canonical_url = cfg.canonical_origin.clone();
    let mut og = OpenGraph::default_for(
        &format!("{BRAND}: uptime monitoring, status pages and on-call"),
        &canonical_url,
        &cfg.canonical_origin,
    );
    og.description = "Hosted uptime monitoring for websites and APIs, with multi-region checks, on-call rotations, incidents, and public status pages. Start free; open source.".to_string();
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

pub async fn landing(State(cfg): State<Arc<MarketingCfg>>, headers: HeaderMap) -> Response {
    let cached = LANDING_CACHED.get_or_init(|| render_landing(&cfg));
    let markdown = LANDING_MD.get_or_init(|| render_landing_markdown(&cfg));
    negotiate::serve(&headers, cached, markdown, &PAGE_CACHE_CONTROL)
}

/// The landing page has no Markdown source of its own — it is a template.
/// `llms.txt` is the authored Markdown statement of the same thing, so an
/// agent asking for Markdown at `/` gets the site index rather than a
/// stripped-down rendering of the hero.
fn render_landing_markdown(cfg: &MarketingCfg) -> CachedRender {
    let body = site_files::llms_markdown(cfg);
    CachedRender {
        etag: body_etag(&String::from_utf8_lossy(&body)),
        body,
    }
}

pub(crate) fn warm(cfg: &MarketingCfg) {
    LANDING_CACHED.get_or_init(|| render_landing(cfg));
    LANDING_MD.get_or_init(|| render_landing_markdown(cfg));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::marketing::pages::{render_architecture, render_not_found};

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

    fn architecture_html(canonical_origin: &str) -> String {
        String::from_utf8(
            render_architecture(&cfg_for(canonical_origin))
                .body
                .to_vec(),
        )
        .expect("utf8 body")
    }

    fn not_found_html(canonical_origin: &str) -> String {
        String::from_utf8(render_not_found(&cfg_for(canonical_origin)).body.to_vec())
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
}
