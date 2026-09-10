//! Operator-controlled branding, resolved for rendering.

use axum::http::HeaderMap;

use crate::app::AppState;
use crate::config::PublicStatusConfig;
use crate::domain::{OrgId, PublicOrgBranding, StatusPageId};
use crate::storage::orgs::{OrgBranding, load_page_branding};
use crate::web::host::is_subdomain_public_request;

use super::urls::LOGO_ROUTE;

/// Operator-controlled branding, resolved for rendering. Optional DB fields
/// have already had their defaults applied here, so the template just prints
/// these values — no fallback logic in the template layer.
pub struct BrandingView {
    pub display_name: String,
    /// Pre-sanitised HTML (markdown → ammonia allow-list). Rendered with the
    /// `safe` filter; it is the *only* unescaped value on the page.
    pub about_html: Option<String>,
    /// Already passed through [`safe_brand_color`]; safe to interpolate into
    /// the `:root` `<style>` block verbatim.
    pub brand_color: String,
    pub brand_text: &'static str,
    pub logo_url: Option<String>,
    pub show_powered_by: bool,
    pub style: &'static str,
    /// Where the status page lives on this host: every self-link and its
    /// canonical URL use it. See [`status_home`].
    pub home: &'static str,
    pub hide_from_search: bool,
    /// Operator's own site. The header brand links here when set, else [`Self::home`].
    pub website_url: Option<String>,
    /// Whether that link passes ranking signal. See [`enforce_follow_link`].
    pub follow_website_link: bool,
}

impl BrandingView {
    /// The header brand links to the operator's own site when they set one.
    pub fn brand_href(&self) -> &str {
        self.website_url.as_deref().unwrap_or(self.home)
    }

    /// `rel` for the header brand link, or `None` where it needs none: a link
    /// to the page's own root, or an outbound one the plan lets pass signal.
    /// The anchor opens in the same tab, so `noopener` would carry nothing.
    pub fn brand_rel(&self) -> Option<&'static str> {
        match self.website_url {
            Some(_) if !self.follow_website_link => Some("nofollow"),
            _ => None,
        }
    }

    pub fn robots(&self) -> &'static str {
        if self.hide_from_search {
            "noindex,follow"
        } else {
            "index,follow"
        }
    }

    pub(super) fn from_org(o: &OrgBranding, cfg: &PublicStatusConfig, home: &'static str) -> Self {
        let display_name = o.resolved_display_name().to_owned();
        let about_html = o
            .branding
            .public_about
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(render_about);
        let brand_color = safe_brand_color(
            o.branding.public_brand_color.as_deref(),
            &cfg.default_brand_color,
        );
        let brand_text = safe_brand_text_for(&brand_color);
        BrandingView {
            display_name,
            about_html,
            brand_color,
            brand_text,
            logo_url: o
                .branding
                .logo_hash
                .as_deref()
                .map(|h| format!("{LOGO_ROUTE}?v={h}")),
            show_powered_by: o.branding.show_powered_by(cfg.default_show_powered_by),
            style: o.branding.public_style.as_str(),
            home,
            hide_from_search: o.branding.public_hide_from_search,
            website_url: o.branding.public_website_url.clone(),
            follow_website_link: false,
        }
    }
}

/// Loads the org's branding for a rendered page. A missing row, missing DB
/// handle, or query error degrades to defaults keyed off `fallback_name` —
/// the status page must still render if branding can't be read.
pub(super) async fn resolve_branding(
    state: &AppState,
    headers: &HeaderMap,
    org: OrgId,
    page: StatusPageId,
    fallback_name: &str,
) -> BrandingView {
    let cfg = &state.cfg.public_status;
    let home = status_home(state, headers);
    let mut view = if let Some(pool) = state.db.as_ref()
        && let Ok(Some(ob)) = load_page_branding(pool, page).await
    {
        BrandingView::from_org(&ob, cfg, home)
    } else {
        // Branding is also where the page says whether it may be indexed, so an
        // unreadable one keeps the generic render out of search results.
        BrandingView::from_org(
            &OrgBranding {
                name: fallback_name.to_owned(),
                slug: String::new(),
                branding: PublicOrgBranding {
                    public_hide_from_search: true,
                    ..PublicOrgBranding::default()
                },
            },
            cfg,
            home,
        )
    };
    // On a plan-lookup fault, fail closed (badge shown, link nofollowed) but log
    // it — otherwise a DB blip silently strips a paying Pro page's white-label
    // with no signal.
    let white_label = match state.quotas.limit_for_org(org).await {
        Ok(p) => p.white_label_enabled,
        Err(e) => {
            tracing::warn!(
                error = %e,
                org = %org.0,
                "white-label gate: plan lookup failed; showing badge and nofollowing the site link"
            );
            false
        }
    };
    view.show_powered_by = enforce_powered_by(
        view.show_powered_by,
        state.cfg.marketing.enabled,
        white_label,
    );
    view.follow_website_link = enforce_follow_link(
        state.cfg.marketing.enabled,
        white_label,
        view.hide_from_search,
    );
    view
}

/// A tenant subdomain serves the status page at its root, which is the URL
/// customers hand out, so that is the one to link and to declare canonical.
/// A path-based deploy keeps `/status`, its root being the operator dashboard.
pub(super) fn status_home(state: &AppState, headers: &HeaderMap) -> &'static str {
    if is_subdomain_public_request(state, headers) {
        "/"
    } else {
        "/status"
    }
}

/// White-label is Pro-only: on SaaS a plan without white-label always shows the
/// "powered by" badge, whatever the page stored. Self-host (marketing off) and
/// white-label plans keep the stored preference.
pub(super) fn enforce_powered_by(stored: bool, saas: bool, white_label: bool) -> bool {
    if saas && !white_label { true } else { stored }
}

/// Whether the header's outbound link passes ranking signal. Hosted has open
/// signup, so a free page's link is worth spamming for and the plan has to sell
/// white-label — the same flag, on purpose: a tier that gets its own branding is
/// the tier whose link we vouch for. Self-host has nobody to police. A page kept
/// out of search vouches for nothing either way.
pub(super) fn enforce_follow_link(saas: bool, white_label: bool, hide_from_search: bool) -> bool {
    if hide_from_search {
        return false;
    }
    !saas || white_label
}

/// Independent, template-side re-validation of the brand colour. Trusts
/// neither the DB CHECK constraint nor the app-level validator: it owns its
/// own `^#[0-9a-fA-F]{6}$` predicate and returns `default` on any mismatch, so
/// the value interpolated into the `<style>` block can never break out of the
/// CSS rule even if an upstream layer's predicate is later widened.
pub fn safe_brand_color(raw: Option<&str>, default: &str) -> String {
    fn is_strict_hex(s: &str) -> bool {
        let b = s.as_bytes();
        b.len() == 7 && b[0] == b'#' && b[1..].iter().all(u8::is_ascii_hexdigit)
    }
    match raw {
        Some(c) if is_strict_hex(c) => c.to_owned(),
        _ => default.to_owned(),
    }
}

/// Pick whichever of `#ffffff` or `#0f172a` gives the higher WCAG contrast
/// ratio against the brand. Threshold-by-luminance breaks for mid-tone
/// brands like Twitter blue or Slack yellow (white passes AA-Large but fails
/// AA on small text).
pub fn safe_brand_text_for(brand_hex: &str) -> &'static str {
    const WHITE: &str = "#ffffff";
    const DARK: &str = "#0f172a";
    const L_WHITE: f32 = 1.0;
    // Pre-computed relative luminance of #0f172a.
    const L_DARK: f32 = 0.0145;

    fn srgb_to_linear(c: u8) -> f32 {
        let s = f32::from(c) / 255.0;
        if s <= 0.04045 {
            s / 12.92
        } else {
            ((s + 0.055) / 1.055).powf(2.4)
        }
    }
    let b = brand_hex.as_bytes();
    if b.len() != 7 || b[0] != b'#' {
        return WHITE;
    }
    let Ok(rgb) = u32::from_str_radix(brand_hex.trim_start_matches('#'), 16) else {
        return WHITE;
    };
    let r = srgb_to_linear(((rgb >> 16) & 0xff) as u8);
    let g = srgb_to_linear(((rgb >> 8) & 0xff) as u8);
    let bl = srgb_to_linear((rgb & 0xff) as u8);
    let lum = 0.2126 * r + 0.7152 * g + 0.0722 * bl;
    let contrast_white = (L_WHITE + 0.05) / (lum + 0.05);
    let contrast_dark = (lum + 0.05) / (L_DARK + 0.05);
    if contrast_dark > contrast_white {
        DARK
    } else {
        WHITE
    }
}

/// Allow-list sanitiser for `public_about`. Built once: the tag set and
/// builder are immutable, and `clean` takes `&self`, so there's no reason to
/// reconstruct it per render.
static ABOUT_SANITIZER: std::sync::LazyLock<ammonia::Builder<'static>> =
    std::sync::LazyLock::new(|| {
        let mut b = ammonia::Builder::default();
        b.tags(
            ["p", "strong", "em", "a", "br", "ul", "ol", "li"]
                .into_iter()
                .collect(),
        )
        .link_rel(Some("noopener nofollow"));
        b
    });

/// Renders `public_about` markdown to HTML, then strips everything outside a
/// small allow-list. The parser output is never trusted: ammonia is the
/// boundary, not pulldown-cmark.
pub fn render_about(markdown: &str) -> String {
    let parser = pulldown_cmark::Parser::new(markdown);
    let mut html = String::new();
    pulldown_cmark::html::push_html(&mut html, parser);
    ABOUT_SANITIZER.clean(&html).to_string()
}
