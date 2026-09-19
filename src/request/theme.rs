//! `sm_theme` cookie helper. The cookie is a fast-path mirror of `users.theme`
//! that the inline boot script in `templates/base.html` reads BEFORE the
//! stylesheet parses, so the user's chosen theme is the very first render.
//! The DB column remains the source of truth.

use tower_cookies::Cookie;
use tower_cookies::cookie::SameSite;
use tower_cookies::cookie::time::Duration;

use crate::domain::AppTheme;

pub const COOKIE_NAME: &str = "sm_theme";

/// `http_only=false` is intentional — the inline boot script reads
/// `document.cookie` to apply the theme class before the stylesheet parses.
/// Issued for a fresh browser at login by
/// [`crate::request::display_prefs::issue_cookies`].
pub fn build_cookie(theme: AppTheme, secure: bool) -> Cookie<'static> {
    let mut c = Cookie::new(COOKIE_NAME, theme.as_str().to_owned());
    c.set_http_only(false);
    c.set_secure(secure);
    c.set_same_site(SameSite::Lax);
    c.set_path("/");
    c.set_max_age(Duration::days(365));
    c
}
