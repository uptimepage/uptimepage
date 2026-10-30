//! Operator-controlled branding, resolved for rendering on every page-host view:
//! the status page, its sign-in screen, subscribe, and shared monitor links.

use axum::http::HeaderMap;

use crate::app::AppState;
use crate::config::PublicStatusConfig;
use crate::domain::{OrgId, PublicOrgBranding, StatusPageId};
use crate::request::host::is_subdomain_public_request;
use crate::storage::orgs::{OrgBranding, load_page_branding};

use crate::domain::Locale;
use crate::public_status::credit;
use crate::public_status::status_title;
use crate::public_status::urls::LOGO_ROUTE;
use crate::request::host::published_page_origin;

/// Operator-controlled branding, resolved for rendering. Optional DB fields
/// have already had their defaults applied here, so the template just prints
/// these values — no fallback logic in the template layer.
pub struct BrandingView {
    pub display_name: String,
    /// Pre-sanitised HTML (markdown → ammonia allow-list). Rendered with the
    /// `safe` filter.
    pub about_html: Option<String>,
    /// Already passed through `safe_brand_color`; safe to interpolate into
    /// the `:root` `<style>` block verbatim.
    pub brand_color: String,
    pub brand_text: &'static str,
    pub logo_url: Option<String>,
    pub show_powered_by: bool,
    /// Host the page is published on, which the credit link reports as the
    /// page that sent the visit.
    pub credit_host: Option<String>,
    /// The Licenses and Source code footer links. See `enforce_source_links`.
    pub show_source_links: bool,
    pub style: &'static str,
    /// Where the status page lives on this host: every self-link and its
    /// canonical URL use it. See [`status_home`].
    pub home: &'static str,
    pub hide_from_search: bool,
    /// Operator's own site. The header brand links here when set, else [`Self::home`].
    pub website_url: Option<String>,
    /// Whether that link passes ranking signal. See `enforce_follow_link`.
    pub follow_website_link: bool,
}

impl BrandingView {
    pub fn brand_href(&self) -> &str {
        self.website_url.as_deref().unwrap_or(self.home)
    }

    /// `None` for a link to the page's own root, or an outbound one the plan
    /// lets pass signal. The anchor stays in the tab, so `noopener` adds nothing.
    pub fn brand_rel(&self) -> Option<&'static str> {
        match self.website_url {
            Some(_) if !self.follow_website_link => Some("nofollow"),
            _ => None,
        }
    }

    pub fn status_title(&self) -> String {
        status_title(&self.display_name)
    }

    pub fn credit_url(&self, locale: Locale) -> String {
        credit::url(locale, credit::Surface::Page, self.credit_host.as_deref())
    }

    pub fn robots(&self) -> &'static str {
        if self.hide_from_search {
            "noindex,follow"
        } else {
            "index,follow"
        }
    }

    fn from_org(o: &OrgBranding, cfg: &PublicStatusConfig, home: &'static str) -> Self {
        let display_name = o.resolved_display_name().to_owned();
        let about_html = o
            .branding
            .public_about
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(crate::markdown::about_html);
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
            credit_host: None,
            show_source_links: true,
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
pub async fn resolve_branding(
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
        // Unreadable branding keeps the generic render out of search results.
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
    let enforced = state.cfg.quotas.enforce_plan_features;
    view.show_powered_by = credit::shown(view.show_powered_by, enforced, white_label);
    // A self-hosted page's host is the operator's, not ours to collect.
    if enforced {
        view.credit_host = published_page_origin(&state.request_state(), headers, page)
            .as_deref()
            .and_then(credit::host_of);
    }
    view.show_source_links = enforce_source_links(enforced, view.show_powered_by);
    view.follow_website_link = enforce_follow_link(enforced, white_label, view.hide_from_search);
    view
}

/// A tenant subdomain serves the status page at its root, which is the URL
/// customers hand out, so that is the one to link and to declare canonical.
/// A path-based deploy keeps `/status`, its root being the operator dashboard.
pub(crate) fn status_home(state: &AppState, headers: &HeaderMap) -> &'static str {
    if is_subdomain_public_request(&state.request_state(), headers) {
        "/"
    } else {
        "/status"
    }
}

/// The links go with the enforced badge. A self-host operator running modified
/// code owes its network users the source (AGPL-3.0 section 13), so the toggle
/// never removes them there.
fn enforce_source_links(enforced: bool, show_powered_by: bool) -> bool {
    !enforced || show_powered_by
}

/// Hosted signup is open, so a followed link needs a plan that sells
/// white-label. Self-host has nobody to police, and a page kept out of search
/// passes nothing either way.
fn enforce_follow_link(enforced: bool, white_label: bool, hide_from_search: bool) -> bool {
    if hide_from_search {
        return false;
    }
    !enforced || white_label
}

/// Independent, template-side re-validation of the brand colour. Trusts
/// neither the DB CHECK constraint nor the app-level validator: it owns its
/// own `^#[0-9a-fA-F]{6}$` predicate and returns `default` on any mismatch, so
/// the value interpolated into the `<style>` block can never break out of the
/// CSS rule even if an upstream layer's predicate is later widened.
fn safe_brand_color(raw: Option<&str>, default: &str) -> String {
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
fn safe_brand_text_for(brand_hex: &str) -> &'static str {
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

#[cfg(test)]
impl BrandingView {
    /// Acme's page under the default config, served at `/`.
    pub(crate) fn sample(branding: PublicOrgBranding) -> Self {
        Self::from_org(
            &OrgBranding {
                name: "Acme".into(),
                slug: "acme".into(),
                branding,
            },
            &PublicStatusConfig::default(),
            "/",
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_links_follow_the_badge_only_on_saas() {
        assert!(enforce_source_links(true, true));
        assert!(!enforce_source_links(true, false));
        assert!(enforce_source_links(false, true));
        assert!(enforce_source_links(false, false));
    }

    #[test]
    fn safe_brand_color_accepts_strict_hex_only() {
        assert_eq!(safe_brand_color(Some("#a1B2c3"), "#000000"), "#a1B2c3");
        for bad in [
            "blue",
            "#abc",
            "#3b82f",
            "#3b82f60",
            "#zzzzzz",
            "  #3b82f6",
            "red; } body { display: none }",
        ] {
            assert_eq!(
                safe_brand_color(Some(bad), "#3b82f6"),
                "#3b82f6",
                "should reject {bad:?}"
            );
        }
        assert_eq!(safe_brand_color(None, "#3b82f6"), "#3b82f6");
    }

    #[test]
    fn brand_text_picks_white_on_very_dark_brands() {
        for dark in ["#000000", "#1e3a8a", "#064e3b", "#4c1d95", "#7c2d12"] {
            assert_eq!(
                safe_brand_text_for(dark),
                "#ffffff",
                "{dark} should pair with white"
            );
        }
    }

    #[test]
    fn brand_text_picks_dark_on_anything_brighter() {
        // Pastels, mid-tones (Twitter blue, Slack yellow, Tailwind 500s) all
        // give better contrast against near-black than white per WCAG.
        for bright in [
            "#ffffff", "#ffee99", "#facc15", "#bef264", "#fde68a", "#1d9bf0", "#3b82f6", "#06b6d4",
            "#22c55e", "#ecb22e",
        ] {
            assert_eq!(
                safe_brand_text_for(bright),
                "#0f172a",
                "{bright} should pair with near-black"
            );
        }
    }

    #[test]
    fn brand_text_falls_back_to_white_on_malformed() {
        for bad in ["", "blue", "#fff", "#zzzzzz"] {
            assert_eq!(safe_brand_text_for(bad), "#ffffff");
        }
    }

    #[test]
    fn follow_link_needs_white_label_on_hosted_and_nothing_on_self_host() {
        // Hosted: the plan decides.
        assert!(!enforce_follow_link(true, false, false));
        assert!(enforce_follow_link(true, true, false));
        // Self-host: no open signup to police, so the operator's own link follows.
        assert!(enforce_follow_link(false, false, false));
        // A page kept out of search passes nothing, whatever the plan.
        assert!(!enforce_follow_link(true, true, true));
        assert!(!enforce_follow_link(false, false, true));
    }
}
