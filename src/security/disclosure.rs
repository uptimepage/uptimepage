//! The RFC 9116 vulnerability-disclosure file, from `[security.disclosure]`.
//! Every host the operator answers on serves the same document (the app,
//! each tenant subdomain, the MCP host), or none does. No `Canonical`: the tenant hosts cannot all be listed, and a file
//! fetched from a host it does not name is not to be trusted.

use std::fmt::Write;
use std::sync::Arc;

use axum::Router;
use axum::http::HeaderValue;
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use chrono::{DateTime, Days, SecondsFormat, Utc};

use crate::config::DisclosureConfig;

/// The well-known path, shared by both routers and the tenant-host
/// allow-list so they cannot drift apart.
pub const PATH: &str = "/.well-known/security.txt";

/// RFC 9116 §2.5.5 wants `Expires` under a year out. Counted from the day of
/// the request, so a process that outlives a fixed date never serves a lapsed
/// file, and a restart never depends on someone having renewed one.
const EXPIRES_IN_DAYS: u64 = 180;

/// `text/plain; charset=utf-8` per RFC 9116 §3.
const TEXT_PLAIN: HeaderValue = HeaderValue::from_static("text/plain; charset=utf-8");

const CACHE: HeaderValue =
    HeaderValue::from_static("public, max-age=86400, stale-while-revalidate=86400");

/// Every field but `Expires`, rendered once at boot.
#[derive(Debug, Clone)]
pub struct Disclosure(Arc<str>);

impl Disclosure {
    /// RFC 9116 wants an https `Policy`, so any other `policy` is left out.
    pub fn from_config(cfg: &DisclosureConfig, policy: &str) -> Option<Self> {
        if cfg.contact.is_empty() {
            return None;
        }
        let mut fields = String::new();
        for contact in &cfg.contact {
            let _ = writeln!(fields, "Contact: {contact}");
        }
        let policy = if policy.starts_with("https://") {
            policy
        } else {
            ""
        };
        for (name, value) in [
            ("Policy", policy),
            ("Acknowledgments", cfg.acknowledgments.as_str()),
            ("Preferred-Languages", cfg.preferred_languages.as_str()),
        ] {
            if !value.is_empty() {
                let _ = writeln!(fields, "{name}: {value}");
            }
        }
        Some(Self(fields.into()))
    }

    fn render(&self, now: DateTime<Utc>) -> String {
        let expires = (now.date_naive() + Days::new(EXPIRES_IN_DAYS))
            .and_hms_opt(0, 0, 0)
            .expect("midnight exists")
            .and_utc()
            .to_rfc3339_opts(SecondsFormat::Secs, true);
        format!("{}Expires: {expires}\n", self.0)
    }

    fn serve(&self) -> Response {
        (
            [(CONTENT_TYPE, TEXT_PLAIN), (CACHE_CONTROL, CACHE)],
            self.render(Utc::now()),
        )
            .into_response()
    }
}

/// Mounts [`PATH`] when there is a document; otherwise the router's own 404
/// answers it.
pub fn mount<S>(router: Router<S>, disclosure: Option<&Disclosure>) -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    match disclosure.cloned() {
        Some(d) => router.route(PATH, get(move || async move { d.serve() })),
        None => router,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> DisclosureConfig {
        DisclosureConfig {
            contact: vec![
                "mailto:security@example.test".into(),
                "https://example.test/report".into(),
            ],
            acknowledgments: String::new(),
            preferred_languages: "en, de".into(),
        }
    }

    #[test]
    fn an_http_policy_is_left_out() {
        let body = Disclosure::from_config(&cfg(), "http://lvh.me:8080/security-policy")
            .unwrap()
            .render(Utc::now());
        assert!(!body.contains("Policy:"), "{body}");
    }

    #[test]
    fn nobody_to_report_to_means_no_file() {
        assert!(Disclosure::from_config(&DisclosureConfig::default(), "").is_none());
    }

    #[test]
    fn renders_the_configured_fields_and_no_canonical() {
        let now = DateTime::parse_from_rfc3339("2026-10-23T17:45:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let policy = "https://example.test/security-policy";
        let body = Disclosure::from_config(&cfg(), policy).unwrap().render(now);
        assert_eq!(
            body,
            "Contact: mailto:security@example.test\n\
             Contact: https://example.test/report\n\
             Policy: https://example.test/security-policy\n\
             Preferred-Languages: en, de\n\
             Expires: 2027-04-21T00:00:00Z\n"
        );
    }

    /// RFC 9116 §2.5.5: in the future, and under a year out.
    #[test]
    fn expires_is_always_ahead_and_under_a_year_out() {
        let now = Utc::now();
        let body = Disclosure::from_config(&cfg(), "").unwrap().render(now);
        let expires = body
            .lines()
            .find_map(|l| l.strip_prefix("Expires: "))
            .expect("Expires");
        let expires = DateTime::parse_from_rfc3339(expires).unwrap();
        assert!(expires > now + chrono::Duration::days(150));
        assert!(expires < now + chrono::Duration::days(365));
    }
}
