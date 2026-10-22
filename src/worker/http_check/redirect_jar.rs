//! Cookies that redirect responses set, carried to the later hops of one
//! execution and dropped with it.

use std::cmp::Reverse;

use cookie_store::{Cookie, CookieDomain, CookieStore, StoreAction};
use hyper::HeaderMap;
use hyper::header::SET_COOKIE;
use url::Url;

use super::{MAX_COOKIE_HEADER_BYTES, MAX_JAR_COOKIES, MAX_SET_COOKIE_BYTES};

/// Not `Debug`: it holds cookie values, and those never reach a log.
#[derive(Default)]
pub(super) struct RedirectJar {
    store: CookieStore,
    /// What `store` holds, oldest first. RFC 6265 §5.4 breaks path-length
    /// ties by creation time, which the store does not keep.
    created: Vec<CookieKey>,
}

#[derive(PartialEq, Eq)]
struct CookieKey {
    domain: String,
    path: String,
    name: String,
}

impl CookieKey {
    fn of(cookie: &Cookie<'_>) -> Option<Self> {
        Some(Self {
            domain: cookie.domain.as_cow()?.into_owned(),
            path: String::from(&cookie.path),
            name: cookie.name().to_owned(),
        })
    }
}

impl RedirectJar {
    pub(super) fn store_response(&mut self, headers: &HeaderMap, request_url: &Url) {
        // One value at a time: `Expires` dates hold commas, so joined values
        // cannot be split again.
        for value in headers.get_all(SET_COOKIE) {
            if let Ok(raw) = value.to_str() {
                self.insert(raw, request_url);
            }
        }
    }

    /// The only way into the jar. A cookie that fails any step is skipped
    /// alone. Not `store_response_cookies`: it skips the suffix check and logs
    /// cookie values.
    fn insert(&mut self, raw: &str, request_url: &Url) {
        if raw.len() > MAX_SET_COOKIE_BYTES {
            return;
        }
        let Ok(mut cookie) = Cookie::parse(raw, request_url) else {
            return;
        };
        if !secure_rules_hold(&cookie, request_url) {
            return;
        }
        // RFC 6265 §5.3 step 5.
        if let CookieDomain::Suffix(domain) = &cookie.domain
            && is_public_suffix(domain)
        {
            if !cookie.domain.host_is_identical(request_url) {
                return;
            }
            let Ok(host_only) = CookieDomain::host_only(request_url) else {
                return;
            };
            cookie.domain = host_only;
        }
        let Some(key) = CookieKey::of(&cookie) else {
            return;
        };
        let known = self.created.contains(&key);
        if !known && self.created.len() >= MAX_JAR_COOKIES {
            return;
        }
        match self.store.insert(cookie.into_owned(), request_url) {
            Ok(StoreAction::Inserted | StoreAction::UpdatedExisting) if !known => {
                self.created.push(key);
            }
            Ok(StoreAction::ExpiredExisting) => {
                self.store.remove(&key.domain, &key.path, &key.name);
                self.created.retain(|k| *k != key);
            }
            _ => {}
        }
    }

    /// Name/value pairs for `url` in RFC 6265 §5.4 order: longer paths first,
    /// then older first.
    pub(super) fn cookies_for(&self, url: &Url) -> Vec<(&str, &str)> {
        // The store also counts plain http to a loopback host as secure.
        let https = url.scheme() == "https";
        let mut matched: Vec<_> = self
            .store
            .matches(url)
            .into_iter()
            .filter(|cookie| https || !cookie.secure().unwrap_or(false))
            .collect();
        matched.sort_by_cached_key(|cookie| {
            let age =
                CookieKey::of(cookie).and_then(|key| self.created.iter().position(|k| *k == key));
            (Reverse(cookie.path.len()), age)
        });
        matched
            .into_iter()
            .map(|cookie| cookie.name_value())
            .collect()
    }
}

/// What browsers enforce and RFC 6265 predates: a plain-http hop cannot set a
/// `Secure` cookie, nor a `__Secure-` or `__Host-` one, so it cannot plant a
/// cookie for the https hops after it.
fn secure_rules_hold(cookie: &Cookie<'_>, request_url: &Url) -> bool {
    let secure = cookie.secure().unwrap_or(false);
    let from_https = request_url.scheme() == "https";
    let named = |prefix: &str| {
        cookie
            .name()
            .get(..prefix.len())
            .is_some_and(|start| start.eq_ignore_ascii_case(prefix))
    };
    if named("__Host-") {
        return secure && from_https && cookie.domain().is_none() && cookie.path() == Some("/");
    }
    if secure || named("__Secure-") {
        return secure && from_https;
    }
    true
}

/// Only listed suffixes count, the private section included, so an unlisted
/// single label is not one.
fn is_public_suffix(domain: &str) -> bool {
    psl::suffix(domain.as_bytes()).is_some_and(|suffix| suffix.is_known() && suffix == domain)
}

/// One `Cookie` value: the user's own cookies first and untouched, then the
/// jar's in order for as long as they fit the cap. A name the user set
/// replaces every jar cookie of that exact name.
pub(super) fn cookie_header(user: Option<&str>, jar: &[(&str, &str)]) -> Option<String> {
    let user_names: Vec<&str> = user
        .into_iter()
        .flat_map(|cookies| cookies.split(';'))
        .map(|pair| pair.split_once('=').map_or(pair, |(name, _)| name).trim())
        .collect();
    let mut header = user.unwrap_or_default().to_owned();
    for (name, value) in jar.iter().filter(|(name, _)| !user_names.contains(name)) {
        let separator = if header.is_empty() { "" } else { "; " };
        if header.len() + separator.len() + name.len() + 1 + value.len() > MAX_COOKIE_HEADER_BYTES {
            break;
        }
        header.push_str(separator);
        header.push_str(name);
        header.push('=');
        header.push_str(value);
    }
    (user.is_some() || !header.is_empty()).then_some(header)
}

#[cfg(test)]
mod tests {
    use hyper::header::HeaderValue;

    use super::*;

    fn url(raw: &str) -> Url {
        Url::parse(raw).unwrap()
    }

    fn jar_with(set_by: &str, cookies: &[&str]) -> RedirectJar {
        let mut jar = RedirectJar::default();
        let request_url = url(set_by);
        for cookie in cookies {
            jar.insert(cookie, &request_url);
        }
        jar
    }

    fn sent(jar: &RedirectJar, to: &str) -> Vec<(String, String)> {
        jar.cookies_for(&url(to))
            .into_iter()
            .map(|(name, value)| (name.to_owned(), value.to_owned()))
            .collect()
    }

    fn pair(name: &str, value: &str) -> (String, String) {
        (name.to_owned(), value.to_owned())
    }

    #[test]
    fn a_public_suffix_domain_is_refused_public_or_private() {
        let icann = jar_with("http://www.example.co.uk/", &["a=1; Domain=co.uk"]);
        assert!(sent(&icann, "http://www.other.co.uk/").is_empty());
        assert!(sent(&icann, "http://www.example.co.uk/").is_empty());

        let private = jar_with("http://a.github.io/", &["a=1; Domain=github.io"]);
        assert!(sent(&private, "http://b.github.io/").is_empty());
        assert!(sent(&private, "http://a.github.io/").is_empty());
    }

    #[test]
    fn a_suffix_that_is_the_host_itself_stays_on_that_host() {
        let jar = jar_with("http://github.io/", &["a=1; Domain=github.io"]);
        assert_eq!(sent(&jar, "http://github.io/"), [pair("a", "1")]);
        assert!(sent(&jar, "http://a.github.io/").is_empty());
    }

    #[test]
    fn a_registrable_parent_domain_reaches_its_subdomains() {
        let jar = jar_with("http://www.example.co.uk/", &["a=1; Domain=example.co.uk"]);
        assert_eq!(sent(&jar, "http://api.example.co.uk/"), [pair("a", "1")]);
    }

    #[test]
    fn an_unlisted_single_label_is_not_a_suffix() {
        let jar = jar_with("http://www.intranet/", &["a=1; Domain=intranet"]);
        assert_eq!(sent(&jar, "http://api.intranet/"), [pair("a", "1")]);
    }

    #[test]
    fn a_cookie_over_the_size_cap_is_skipped_alone() {
        let at_cap = format!("a={}", "v".repeat(MAX_SET_COOKIE_BYTES - 2));
        let over_cap = format!("b={}", "v".repeat(MAX_SET_COOKIE_BYTES - 1));
        let jar = jar_with("http://example.com/", &[&at_cap, &over_cap, "c=1"]);
        let names: Vec<String> = sent(&jar, "http://example.com/")
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        assert_eq!(names, ["a", "c"]);
    }

    #[test]
    fn the_jar_keeps_the_oldest_cookies_up_to_its_cap_and_still_updates_them() {
        let cookies: Vec<String> = (0..MAX_JAR_COOKIES + 10)
            .map(|i| format!("c{i}=1"))
            .collect();
        let refs: Vec<&str> = cookies.iter().map(String::as_str).collect();
        let mut jar = jar_with("http://example.com/", &refs);
        jar.insert("c0=2", &url("http://example.com/"));

        let sent = sent(&jar, "http://example.com/");
        assert_eq!(sent.len(), MAX_JAR_COOKIES);
        assert_eq!(sent[0], pair("c0", "2"));
        assert_eq!(
            sent[MAX_JAR_COOKIES - 1],
            pair(&format!("c{}", MAX_JAR_COOKIES - 1), "1")
        );
    }

    #[test]
    fn a_deleted_cookie_is_gone_and_frees_its_slot() {
        let cookies: Vec<String> = (0..MAX_JAR_COOKIES).map(|i| format!("c{i}=1")).collect();
        let refs: Vec<&str> = cookies.iter().map(String::as_str).collect();
        let mut jar = jar_with("http://example.com/", &refs);
        let request_url = url("http://example.com/");
        jar.insert("c0=; Max-Age=0", &request_url);
        jar.insert("late=1", &request_url);

        let sent = sent(&jar, "http://example.com/");
        assert!(!sent.iter().any(|(name, _)| name == "c0"));
        assert!(sent.contains(&pair("late", "1")));
    }

    #[test]
    fn longer_paths_go_first_then_older_cookies() {
        let jar = jar_with(
            "http://example.com/",
            &["z=1", "n=root; Path=/", "n=deep; Path=/a", "a=1"],
        );
        assert_eq!(
            sent(&jar, "http://example.com/a/b"),
            [
                pair("n", "deep"),
                pair("z", "1"),
                pair("n", "root"),
                pair("a", "1")
            ]
        );
    }

    #[test]
    fn a_secure_cookie_goes_over_https_only_even_to_loopback() {
        let jar = jar_with("https://127.0.0.1/", &["s=1; Secure", "n=1"]);
        assert_eq!(sent(&jar, "http://127.0.0.1/"), [pair("n", "1")]);
        assert_eq!(
            sent(&jar, "https://127.0.0.1/"),
            [pair("s", "1"), pair("n", "1")]
        );
    }

    #[test]
    fn a_plain_http_hop_cannot_set_a_secure_or_prefixed_cookie() {
        let jar = jar_with(
            "http://example.com/",
            &[
                "s=1; Secure",
                "__Secure-a=1; Secure",
                "__secure-b=1",
                "__Host-c=1; Secure; Path=/",
                "plain=1",
            ],
        );
        assert_eq!(sent(&jar, "https://example.com/"), [pair("plain", "1")]);
    }

    #[test]
    fn prefixed_cookies_need_their_attributes_over_https() {
        let jar = jar_with(
            "https://www.example.com/",
            &[
                "__Secure-ok=1; Secure",
                "__Secure-bare=1",
                "__Host-ok=1; Secure; Path=/",
                "__Host-domain=1; Secure; Path=/; Domain=example.com",
                "__Host-nopath=1; Secure",
            ],
        );
        assert_eq!(
            sent(&jar, "https://www.example.com/"),
            [pair("__Secure-ok", "1"), pair("__Host-ok", "1")]
        );
    }

    #[test]
    fn each_set_cookie_value_is_parsed_on_its_own() {
        let mut headers = HeaderMap::new();
        for value in [
            "a=1; Expires=Wed, 21 Oct 2099 07:28:00 GMT",
            "no pair here",
            "b=2",
        ] {
            headers.append(SET_COOKIE, HeaderValue::from_static(value));
        }
        let mut jar = RedirectJar::default();
        jar.store_response(&headers, &url("http://example.com/"));
        assert_eq!(
            sent(&jar, "http://example.com/"),
            [pair("a", "1"), pair("b", "2")]
        );
    }

    #[test]
    fn a_user_cookie_replaces_jar_cookies_of_that_exact_name_only() {
        let header = cookie_header(
            Some("SID=user; theme=dark"),
            &[
                ("sid", "jar"),
                ("SID", "jar"),
                ("SID", "older"),
                ("lang", "en"),
            ],
        );
        assert_eq!(
            header.as_deref(),
            Some("SID=user; theme=dark; sid=jar; lang=en")
        );
    }

    #[test]
    fn no_cookies_on_either_side_sends_no_header() {
        assert_eq!(cookie_header(None, &[]), None);
        assert_eq!(cookie_header(Some("a=1"), &[]).as_deref(), Some("a=1"));
        assert_eq!(cookie_header(None, &[("a", "1")]).as_deref(), Some("a=1"));
    }

    #[test]
    fn jar_cookies_that_do_not_fit_are_dropped_from_the_end() {
        let user = "u=".to_owned() + &"x".repeat(MAX_COOKIE_HEADER_BYTES - 20);
        let header = cookie_header(
            Some(&user),
            &[("a", "1"), ("b", "x".repeat(30).as_str()), ("c", "1")],
        )
        .unwrap();
        assert_eq!(header, format!("{user}; a=1"));
        assert!(header.len() <= MAX_COOKIE_HEADER_BYTES);
    }

    #[test]
    fn a_user_cookie_over_the_cap_goes_out_unchanged() {
        let user = "u=".to_owned() + &"x".repeat(MAX_COOKIE_HEADER_BYTES);
        assert_eq!(
            cookie_header(Some(&user), &[("a", "1")]).as_deref(),
            Some(user.as_str())
        );
    }
}
