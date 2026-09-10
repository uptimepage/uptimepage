//! Crawl directives for responses that have no `<head>` to carry a robots meta
//! tag: feeds, badges, JSON, and the polled HTML partials.

use axum::http::{HeaderName, HeaderValue};

pub const X_ROBOTS_TAG: HeaderName = HeaderName::from_static("x-robots-tag");

/// Out of the index, but crawlable through to the page that should rank.
pub const NOINDEX_FOLLOW: HeaderValue = HeaderValue::from_static("noindex,follow");

/// Matches what `share/base.html` puts in its own head.
pub const NOINDEX_NOFOLLOW: HeaderValue = HeaderValue::from_static("noindex, nofollow");
