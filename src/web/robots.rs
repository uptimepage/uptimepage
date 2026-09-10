//! Crawl directives for responses that have no `<head>` to carry a robots meta
//! tag: feeds, badges, JSON, and the polled HTML partials.

use axum::http::{HeaderName, HeaderValue};

pub const X_ROBOTS_TAG: HeaderName = HeaderName::from_static("x-robots-tag");

/// Keep the URL out of the index, but let a crawler follow it to the page that
/// should rank instead.
pub const NOINDEX_FOLLOW: HeaderValue = HeaderValue::from_static("noindex,follow");

/// Matches what `share/base.html` puts in its own head: a share link is nobody's
/// entry point.
pub const NOINDEX_NOFOLLOW: HeaderValue = HeaderValue::from_static("noindex, nofollow");
