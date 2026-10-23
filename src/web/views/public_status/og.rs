//! OG/Twitter card metadata for the public surface.

use axum::http::HeaderMap;

use crate::app::AppState;
use crate::domain::StatusPageId;
use crate::request::host::published_page_origin;

use crate::web::views::page_branding::BrandingView;

/// OG/Twitter card metadata for the public status surface. Empty `url` /
/// `image` / `site_name` mean "skip that tag" — fine for self-hosted setups
/// where the site origin isn't configured.
#[derive(Default)]
pub struct OgMeta {
    pub title: String,
    pub description: String,
    pub og_type: &'static str,
    pub url: String,
    pub image: String,
    /// The tenant's brand, never ours.
    pub site_name: String,
}

/// Builds OG/Twitter metadata for the public surface. `og:url` comes from the
/// resolved page, not the request: it is the page's activated custom domain,
/// else the request Host but only where that validates as the apex or a tenant
/// subdomain of `public_status.base_domain`. Without that gate, an attacker
/// hitting the page with `Host: evil.com` would poison the scraper cache so
/// social shares of legitimate URLs unfurl with attacker's domain. `og:image`
/// degrades to empty (template skips the tag) when the site origin
/// isn't configured.
#[allow(clippy::too_many_arguments)]
pub(super) fn build_og_meta(
    state: &AppState,
    headers: &HeaderMap,
    page: StatusPageId,
    path: &str,
    title: String,
    description: String,
    og_type: &'static str,
    branding: &BrandingView,
) -> OgMeta {
    let url = published_page_origin(&state.request_state(), headers, page)
        .map(|origin| format!("{origin}{path}"))
        .unwrap_or_default();

    let image = og_image(&state.cfg.site.origin);

    OgMeta {
        title,
        description,
        og_type,
        url,
        image,
        site_name: branding.display_name.clone(),
    }
}

/// Never the marketing card: it carries a sign-up CTA, and this page is the
/// tenant's.
pub(super) fn og_image(site_origin: &str) -> String {
    if site_origin.is_empty() {
        String::new()
    } else {
        format!("{site_origin}/static/marketing/og-status.png")
    }
}
