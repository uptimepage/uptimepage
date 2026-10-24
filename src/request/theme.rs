//! Theme cookies. `sm_app_theme` mirrors `users.theme` for the scripts that
//! paint before the stylesheet loads (`templates/base.html`, the site's
//! `theme.js`). `sm_theme_pick` is a light/dark choice from the site's toggle
//! that a new account starts on; it never touches an existing one.

use tower_cookies::cookie::SameSite;
use tower_cookies::cookie::time::Duration;
use tower_cookies::{Cookie, Cookies};

use crate::config::AppConfig;
use crate::domain::AppTheme;

pub const COOKIE_NAME: &str = "sm_app_theme";
pub const PICK_COOKIE_NAME: &str = "sm_theme_pick";

/// With a site on the base domain the mirror takes that domain, so both
/// surfaces read it; host-only otherwise.
#[derive(Debug, Clone, Copy)]
pub struct CookieScope<'a> {
    pub secure: bool,
    pub domain: Option<&'a str>,
}

impl<'a> CookieScope<'a> {
    pub fn from_config(cfg: &'a AppConfig) -> Self {
        let base = cfg.public_status.base_domain.trim();
        Self {
            secure: cfg.auth.session.cookie_secure,
            domain: (!base.is_empty() && cfg.site.host().as_deref() == Some(base)).then_some(base),
        }
    }
}

/// `http_only=false` is intentional: scripts read it.
pub fn build_cookie(theme: AppTheme, scope: CookieScope<'_>) -> Cookie<'static> {
    let mut c = Cookie::new(COOKIE_NAME, theme.as_str());
    c.set_http_only(false);
    c.set_secure(scope.secure);
    c.set_same_site(SameSite::Lax);
    c.set_path("/");
    if let Some(domain) = scope.domain {
        c.set_domain(domain.to_owned());
    }
    c.set_max_age(Duration::days(365));
    c
}

/// Light is [`AppTheme::Default`], the look the marketing light palette mirrors.
pub fn pending_pick(cookies: &Cookies) -> Option<AppTheme> {
    match cookies.get(PICK_COOKIE_NAME)?.value() {
        "light" => Some(AppTheme::Default),
        "dark" => Some(AppTheme::Dark),
        _ => None,
    }
}

/// The themes `static/css/input.css` paints dark, which the site's theme
/// script must also know.
#[cfg(test)]
pub(crate) fn dark_app_themes() -> Vec<String> {
    let css = include_str!("../../static/css/input.css");
    let (before, _) = css
        .split_once("{ color-scheme: dark; }")
        .expect("static/css/input.css has a `{ color-scheme: dark; }` rule");
    let rule = before.rsplit('}').next().unwrap_or(before);
    let mut themes: Vec<String> = rule
        .split(',')
        .filter_map(|sel| sel.trim().strip_prefix(".theme-"))
        .map(str::to_owned)
        .collect();
    themes.sort();
    themes
}

#[cfg(test)]
mod tests {
    use crate::domain::AppTheme;

    #[test]
    fn every_theme_the_stylesheet_paints_dark_is_an_app_theme() {
        let dark = super::dark_app_themes();
        assert!(!dark.is_empty());
        assert!(
            dark.iter().all(|t| AppTheme::ALL.contains(&t.as_str())),
            "{dark:?}"
        );
    }
}
