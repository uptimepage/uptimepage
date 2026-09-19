//! One-shot cookie carrying the purge date from `DELETE /api/v1/me` to the
//! signed-out confirmation page.
//!
//! The deletion drops every session, so that page cannot read the date from the
//! database — there is no longer anyone to read it as. The date is not a secret
//! and a forged one only mis-labels an informational page, so a plain
//! short-lived cookie is the whole mechanism.

use chrono::{DateTime, Utc};
use tower_cookies::Cookie;
use tower_cookies::Cookies;
use tower_cookies::cookie::SameSite;
use tower_cookies::cookie::time::Duration;

const COOKIE_NAME: &str = "_sm_deleted";
// Survives the post-delete redirect with room for a slow hand-off, and expires
// long before the page could resurface on a later visit.
const TTL_SECS: i64 = 300;

/// `domain` must match the session cookie's (empty = host-only) so the redirect
/// can't drop it.
pub fn set(cookies: &Cookies, purge_at: DateTime<Utc>, secure: bool, domain: &str) {
    let mut c = Cookie::new(COOKIE_NAME, purge_at.to_rfc3339());
    c.set_http_only(true);
    c.set_secure(secure);
    c.set_same_site(SameSite::Lax);
    c.set_path("/");
    if !domain.is_empty() {
        c.set_domain(domain.to_owned());
    }
    c.set_max_age(Duration::seconds(TTL_SECS));
    cookies.add(c);
}

/// Read and consume. An unparseable value reads as absent, so the page renders
/// without a date rather than failing.
pub fn take(cookies: &Cookies, domain: &str) -> Option<DateTime<Utc>> {
    let c = cookies.get(COOKIE_NAME)?;
    let parsed = DateTime::parse_from_rfc3339(c.value())
        .ok()
        .map(|t| t.with_timezone(&Utc));
    let mut gone = Cookie::new(COOKIE_NAME, "");
    gone.set_path("/");
    if !domain.is_empty() {
        gone.set_domain(domain.to_owned());
    }
    cookies.remove(gone);
    parsed
}
