//! Persistent "last sign-in method" hint cookie. Lets the login page mark the
//! provider a returning visitor used last — a recognition cue only. It never
//! gates anything: the real check stays the OAuth dance and the session cookie.

use tower_cookies::Cookie;
use tower_cookies::Cookies;
use tower_cookies::cookie::SameSite;
use tower_cookies::cookie::time::Duration;

use crate::config::SessionConfig;

const COOKIE_NAME: &str = "_sm_last_method";
const TTL_DAYS: i64 = 180;

/// Record the method just used. Scope is taken from the session config so the
/// hint always rides the same redirects and subdomain hops as the session
/// cookie — they must share `secure`/`domain` or one is dropped where the
/// other survives.
pub fn set(cookies: &Cookies, session: &SessionConfig, method: &str) {
    let mut c = Cookie::new(COOKIE_NAME, method.to_owned());
    c.set_http_only(true);
    c.set_secure(session.cookie_secure);
    c.set_same_site(SameSite::Lax);
    c.set_path("/");
    if !session.cookie_domain.is_empty() {
        c.set_domain(session.cookie_domain.clone());
    }
    c.set_max_age(Duration::days(TTL_DAYS));
    cookies.add(c);
}

/// Read the hint for the login page. Non-consuming — the badge persists across
/// visits until a different method overwrites it.
pub fn get(cookies: &Cookies) -> Option<String> {
    cookies.get(COOKIE_NAME).map(|c| c.value().to_owned())
}
