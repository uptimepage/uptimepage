//! The RFC 9116 vulnerability-disclosure file.
//!
//! One document, one handler, four host families: the marketing apex
//! (the `Canonical` origin), the operator app, every public tenant
//! subdomain (`PUBLIC_TENANT_EXACT` in `request::host`) and the MCP
//! connector host. A researcher who lands on any of them should not
//! have to guess where to report, and each host answering identically
//! is what keeps that promise cheap.

use axum::http::HeaderValue;
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE};
use axum::response::{IntoResponse, Response};

/// The well-known path, shared by both routers and the tenant-host
/// allow-list so they cannot drift apart.
pub const PATH: &str = "/.well-known/security.txt";

pub const BODY: &str = include_str!("../../static/.well-known/security.txt");

/// `text/plain; charset=utf-8` per RFC 9116 §3.
const TEXT_PLAIN: HeaderValue = HeaderValue::from_static("text/plain; charset=utf-8");

/// Matches the legal pages: the file changes about once a year, and a
/// stale copy in an intermediary is bounded by `Expires` anyway.
const CACHE: HeaderValue =
    HeaderValue::from_static("public, max-age=86400, stale-while-revalidate=86400");

/// The single handler every router mounts at [`PATH`]. Takes no state,
/// so it fits the marketing and app routers alike — two copies would
/// drift on headers before they drifted on bytes.
pub async fn serve() -> Response {
    ([(CONTENT_TYPE, TEXT_PLAIN), (CACHE_CONTROL, CACHE)], BODY).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The apex is the `Canonical` origin; every other host serves a
    /// copy of these same bytes.
    const CANONICAL_ORIGIN: &str = "https://uptimepage.dev";

    fn field(name: &str) -> &'static str {
        BODY.lines()
            .find_map(|l| l.trim_end().strip_prefix(&format!("{name}: ")))
            .unwrap_or_else(|| panic!("security.txt lost the {name} field"))
    }

    #[test]
    fn carries_the_required_fields() {
        for name in ["Contact", "Expires", "Canonical", "Policy"] {
            assert!(!field(name).is_empty(), "{name} is present but empty");
        }
    }

    #[test]
    fn canonical_names_the_apex_well_known_url() {
        assert_eq!(field("Canonical"), format!("{CANONICAL_ORIGIN}{PATH}"));
    }

    /// The live invariant, kept next to the data so the `--lib` gate
    /// catches a stale file. RFC 9116 §2.5.5: a past `Expires` means
    /// the file MUST NOT be trusted, so the test reddens a month early
    /// rather than on the day production starts serving a dead one.
    #[test]
    fn expires_is_renewed_early_and_under_a_year_out() {
        let expires = chrono::DateTime::parse_from_rfc3339(field("Expires"))
            .expect("Expires must be an RFC 3339 timestamp")
            .with_timezone(&chrono::Utc);
        let now = chrono::Utc::now();
        assert!(
            expires > now + chrono::Duration::days(30),
            "security.txt expires {expires} — renew it now, before production serves a dead file"
        );
        // 366, not 365: a renewal one calendar year out can span a leap day.
        assert!(
            expires - now <= chrono::Duration::days(366),
            "RFC 9116 §2.5.5 wants Expires under a year out, got {expires}"
        );
    }
}
