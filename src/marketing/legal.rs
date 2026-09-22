//! Public legal & policy pages on the marketing host:
//! `/terms`, `/privacy`, `/cookies`, `/impressum`, `/abuse-policy`,
//! `/security-policy`, plus the RFC 9116 `/.well-known/security.txt`,
//! whose `Canonical` field names this host. Markdown sources under
//! `docs/legal/` are
//! compiled into the binary with `include_str!` and rendered to HTML
//! once on first request, then memoised in a `OnceLock` per route.
//!
//! The renderer is deliberately **unsanitised** — first-party legal
//! content uses tables and the occasional raw `<a>` that ammonia would
//! strip. The blog renderer (third-party PR input) is a separate
//! sanitised path and must not be reused here.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, OnceLock};

use askama::Template;
use askama_web::WebTemplate;
use axum::Router;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::HeaderValue;
use axum::response::Response;
use axum::routing::get;

use super::config::{BRAND, MarketingCfg};
use super::pages::{CachedRender, cached_render, serve_cached};
use super::seo::OpenGraph;
use crate::security::disclosure;
use crate::templates::filters;

const LEGAL_CACHE_CONTROL: HeaderValue =
    HeaderValue::from_static("public, max-age=86400, stale-while-revalidate=86400");

fn render_trusted_unsanitised(markdown: &str) -> String {
    let mut opts = pulldown_cmark::Options::empty();
    opts.insert(pulldown_cmark::Options::ENABLE_TABLES);
    let parser = pulldown_cmark::Parser::new_ext(markdown, opts);
    let mut html = String::new();
    pulldown_cmark::html::push_html(&mut html, super::md::wrap_tables(parser));
    html
}

#[derive(Template, WebTemplate)]
#[template(path = "marketing/legal.html")]
struct LegalPage {
    title: &'static str,
    body: &'static str,
    canonical_url: String,
    og: OpenGraph,
    app_url: String,
    version: &'static str,
}

static TERMS: LazyLock<String> =
    LazyLock::new(|| render_trusted_unsanitised(include_str!("../../docs/legal/terms.md")));
static PRIVACY: LazyLock<String> =
    LazyLock::new(|| render_trusted_unsanitised(include_str!("../../docs/legal/privacy.md")));
static COOKIES: LazyLock<String> =
    LazyLock::new(|| render_trusted_unsanitised(include_str!("../../docs/legal/cookies.md")));
static IMPRESSUM: LazyLock<String> =
    LazyLock::new(|| render_trusted_unsanitised(include_str!("../../docs/legal/impressum.md")));
static ABUSE: LazyLock<String> =
    LazyLock::new(|| render_trusted_unsanitised(include_str!("../../docs/legal/abuse-policy.md")));
static SECURITY: LazyLock<String> = LazyLock::new(|| {
    render_trusted_unsanitised(include_str!("../../docs/legal/security-policy.md"))
});
static BOT: LazyLock<String> =
    LazyLock::new(|| render_trusted_unsanitised(include_str!("../../docs/legal/bot.md")));

pub struct LegalRoute {
    pub path: &'static str,
    pub title: &'static str,
    body: &'static LazyLock<String>,
}

/// Single source of truth for legal pages: router mount, renderer, and
/// sitemap all iterate this slice. Add a page → one entry.
pub const ROUTES: &[LegalRoute] = &[
    LegalRoute {
        path: "/terms",
        title: "Terms of Service",
        body: &TERMS,
    },
    LegalRoute {
        path: "/privacy",
        title: "Privacy Policy",
        body: &PRIVACY,
    },
    LegalRoute {
        path: "/cookies",
        title: "Cookie Policy",
        body: &COOKIES,
    },
    LegalRoute {
        path: "/impressum",
        title: "Impressum",
        body: &IMPRESSUM,
    },
    LegalRoute {
        path: "/abuse-policy",
        title: "Abuse Policy",
        body: &ABUSE,
    },
    LegalRoute {
        path: "/security-policy",
        title: "Security Policy",
        body: &SECURITY,
    },
    LegalRoute {
        path: "/bot",
        title: "Uptimepage Bot",
        body: &BOT,
    },
];

static RENDERED: OnceLock<HashMap<&'static str, CachedRender>> = OnceLock::new();

fn render_all(cfg: &MarketingCfg) -> HashMap<&'static str, CachedRender> {
    ROUTES
        .iter()
        .map(|route| {
            let canonical_url = format!("{}{}", cfg.canonical_origin, route.path);
            let og = OpenGraph::default_for(
                &format!("{} | {BRAND}", route.title),
                &canonical_url,
                &cfg.canonical_origin,
            );
            let page = LegalPage {
                title: route.title,
                body: route.body.as_str(),
                canonical_url,
                og,
                app_url: cfg.app_url.clone(),
                version: env!("CARGO_PKG_VERSION"),
            };
            let body = page
                .render()
                .unwrap_or_else(|e| format!("<!-- legal render failed: {e} -->"));
            (route.path, cached_render(body))
        })
        .collect()
}

/// Warm the per-route render cache at boot so the first hit on any
/// `/terms`-style URL doesn't pay markdown + askama + SHA-256.
pub(crate) fn warm(cfg: &MarketingCfg) {
    RENDERED.get_or_init(|| render_all(cfg));
}

async fn serve(
    State(cfg): State<Arc<MarketingCfg>>,
    headers: HeaderMap,
    route: &'static LegalRoute,
) -> Response {
    let map = RENDERED.get_or_init(|| render_all(&cfg));
    // `render_all` and `mount` both iterate `ROUTES`, so every mounted
    // path is guaranteed present in the map.
    let cached = map.get(route.path).expect("ROUTES drives render");
    serve_cached(&headers, cached, &LEGAL_CACHE_CONTROL)
}

/// Mount every entry in [`ROUTES`] on the given router. One source of
/// truth for the rendered pages — no per-route handler functions, no
/// `match` arm, no macro invocations to keep in lockstep.
///
/// `security.txt` is the one exception, appended after the loop: it is
/// plain text rather than a rendered page, and this host has to answer
/// it because the file names this origin as its `Canonical` one. The
/// handler is shared with the app router so the two cannot drift.
pub fn mount(router: Router<Arc<MarketingCfg>>) -> Router<Arc<MarketingCfg>> {
    let mut r = router;
    for route in ROUTES {
        r = r.route(
            route.path,
            get(move |state, headers| serve(state, headers, route)),
        );
    }
    r.route(disclosure::PATH, get(disclosure::serve))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_route_has_non_empty_body() {
        for route in ROUTES {
            assert!(
                !route.body.is_empty(),
                "{} rendered to empty html",
                route.path
            );
        }
    }

    #[test]
    fn trusted_renderer_preserves_tables() {
        let md = "| a | b |\n|---|---|\n| 1 | 2 |\n";
        let html = render_trusted_unsanitised(md);
        assert!(html.contains("<table>"), "expected <table>, got {html}");
    }
}
