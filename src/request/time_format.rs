//! `sm_time_format` cookie helper — a fast-path mirror of `users.time_format`
//! that `localtime.js` reads (`document.cookie`) to pick the 12h/24h hour cycle
//! when rendering timestamps. The DB column remains the source of truth.

use tower_cookies::Cookie;
use tower_cookies::cookie::SameSite;
use tower_cookies::cookie::time::Duration;

use crate::domain::TimeFormat;

pub const COOKIE_NAME: &str = "sm_time_format";

/// `http_only=false` is intentional — `localtime.js` reads `document.cookie`
/// to choose the hour cycle when it localizes `<time>` elements. Issued for a
/// fresh browser at login by [`crate::request::display_prefs::issue_cookies`].
pub fn build_cookie(fmt: TimeFormat, secure: bool) -> Cookie<'static> {
    let mut c = Cookie::new(COOKIE_NAME, fmt.as_str().to_owned());
    c.set_http_only(false);
    c.set_secure(secure);
    c.set_same_site(SameSite::Lax);
    c.set_path("/");
    c.set_max_age(Duration::days(365));
    c
}
