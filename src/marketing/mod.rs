//! Marketing site & blog served from the apex host.
//!
//! **Hard-isolated by contract:** this module must not import
//! `crate::storage`, `crate::domain`, `crate::tenancy`, `crate::state`,
//! or `AppState`. The dispatch seam (one middleware, one router call —
//! see `marketing::dispatch`) is the entire coupling to the rest of the
//! binary; extracting marketing to its own service is a copy + delete.
//! No-DB on the marketing path is what makes that extraction trivial.
//!
//! Permitted shared deps: axum, askama, tower-http, the config crate,
//! and `crate::templates::assets` (the fingerprinted-asset map + askama
//! filter + `mount_static` helper). On extraction the assets module
//! moves with the marketing site or is duplicated into the extracted
//! service; either way the rebind is one path.
//! The SSL checker adds two more, both leaves that travel the same way:
//! `crate::security::{SsrfGuard, cert_probe}` and
//! `crate::request::client_ip::extract`. Each is a pure function over its
//! arguments — no pool, no state, no resolver — which is the property
//! that keeps them copyable rather than couplings.
//! The domain checker also uses the pure `security::rdap` parser and the
//! bounded, SSRF-guarded `http_outbound` transport, with its own public budget
//! and cache. It does not import the scheduled worker or its durable state.
//! The Markdown renderer is **vendored locally** (`blog::render`) —
//! deliberately not the trusted-unsanitised legal renderer; blog
//! content is third-party PR input and has a different trust model.

pub mod blog;
pub mod changelog;
pub mod config;
pub mod discovery;
pub mod dispatch;
pub mod docs;
pub mod gallery;
mod highlight;
pub mod landings;
pub mod legal;
pub mod md;
mod negotiate;
pub mod pages;
pub mod seo;
pub mod start;
pub mod tools;

use std::sync::Arc;

use axum::Router;
use axum::http::header::LINK;
use axum::routing::get;
use tower_http::compression::CompressionLayer;
use tower_http::set_header::SetResponseHeaderLayer;

pub use config::MarketingCfg;
pub use dispatch::RouteByHost;

/// Build the marketing router. Takes its own config — no `AppState`,
/// no pools. The `CompressionLayer` is scoped here so it never wraps
/// `/healthz`/`/readyz` on the app surface.
///
/// Every page/blog/legal/seo cache is warmed here at boot via
/// [`warm_caches`], so the first real request after startup never pays
/// askama + SHA-256 + (for blog) markdown + ammonia on the hot path.
pub fn router(cfg: MarketingCfg) -> Router {
    let state = Arc::new(cfg);
    warm_caches(&state);
    let mut r = Router::new()
        .route("/", get(pages::landing))
        .route("/pricing", get(pages::pricing))
        .route(pages::ARCHITECTURE_PATH, get(pages::architecture))
        .route(start::START_PATH, get(start::start))
        .route("/robots.txt", get(seo::robots_txt))
        .route("/sitemap.xml", get(seo::sitemap_xml))
        .route("/llms.txt", get(seo::llms_txt))
        .route("/llms-full.txt", get(seo::llms_full_txt))
        .route(discovery::CATALOG_PATH, get(discovery::api_catalog))
        .route(discovery::CARD_PATH, get(discovery::mcp_server_card))
        .route(
            "/startupranking1371476620941810.html",
            get(seo::startupranking_verification),
        );
    r = legal::mount(r);
    r = landings::mount(r);
    r = tools::mount(r);
    r = docs::mount(r);
    r = changelog::mount(r);
    if state.blog_enabled {
        r = r
            .route("/blog", get(blog::index))
            .route("/blog/{slug}", get(blog::post));
    }
    // Wraps only the page routes above; `/static` and the 404 mount after.
    if let Some(link) = discovery::link_header(&state) {
        r = r.layer(SetResponseHeaderLayer::if_not_present(LINK, link));
    }
    // The dispatcher routes the whole apex/www host to this router, so
    // marketing must own a `/static/{*path}` route — otherwise every
    // asset href emitted by a template falls through to the marketing
    // 404. Funnelled through `mount_static` so the path + handler stay
    // in lockstep with the operator-app declaration.
    crate::templates::assets::mount_static(r)
        .fallback(pages::not_found)
        .layer(CompressionLayer::new())
        .with_state(state)
}

/// Eagerly populate every `OnceLock` cache in the marketing module.
/// Each callee is independently idempotent (`OnceLock::get_or_init` +
/// `list_published()` auto-loads posts), so call order is irrelevant —
/// no cross-module ordering contract to keep in lockstep with edits.
fn warm_caches(state: &Arc<MarketingCfg>) {
    pages::warm(state);
    legal::warm(state);
    landings::warm(state);
    tools::warm(state);
    docs::warm(state);
    changelog::warm(state);
    discovery::warm(state);
    if state.blog_enabled {
        blog::warm(state);
    }
    seo::warm(state);
}
