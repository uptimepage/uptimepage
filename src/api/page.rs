use serde::Serialize;
use utoipa::ToSchema;

use crate::domain::metrics::TagCount;
use crate::domain::{CheckResult, Incident, MaintenanceWindow, PublicIncident, Target};
use crate::security::redaction::RedactInPlace;

/// Standard envelope returned by every paginated list endpoint.
///
/// Carries `has_more` instead of `total` so handlers don't need a parallel
/// `count(*)` round-trip just to surface "is there a next page". Endpoints
/// that already needed a count internally can still set it; the wire shape
/// is the same either way.
///
/// utoipa registers a distinct named schema per concrete instantiation. The
/// type aliases below pin each variant so `ApiDoc` can name them stably.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct PageEnvelope<T>
where
    T: ToSchema,
{
    pub items: Vec<T>,
    pub limit: u32,
    pub offset: u32,
    pub has_more: bool,
}

impl<T> PageEnvelope<T>
where
    T: ToSchema,
{
    pub fn new(items: Vec<T>, limit: u32, offset: u32, has_more: bool) -> Self {
        Self {
            items,
            limit,
            offset,
            has_more,
        }
    }

    /// Build from a "fetch limit+1, peek for next page" pattern: if the
    /// caller asked for `limit` rows and the store returned `limit+1`,
    /// there is at least one more page and the extra row is dropped.
    /// Cheaper than computing `total` via a parallel `count(*)`.
    pub fn from_peek(mut peek: Vec<T>, limit: u32, offset: u32) -> Self {
        let has_more = peek.len() as u32 > limit;
        if has_more {
            peek.truncate(limit as usize);
        }
        Self::new(peek, limit, offset, has_more)
    }
}

impl<T> RedactInPlace for PageEnvelope<T>
where
    T: ToSchema + RedactInPlace,
{
    fn redact_in_place(&mut self) {
        for item in &mut self.items {
            item.redact_in_place();
        }
    }
}

pub type PageOfTarget = PageEnvelope<Target>;
pub type PageOfCheckResult = PageEnvelope<CheckResult>;
pub type PageOfIncident = PageEnvelope<Incident>;
pub type PageOfTagCount = PageEnvelope<TagCount>;
pub type PageOfPublicIncident = PageEnvelope<PublicIncident>;
pub type PageOfMaintenanceWindow = PageEnvelope<MaintenanceWindow>;

/// Keyset-pagination envelope for time-ordered lists. Replaces the
/// `total` / `offset` envelope on endpoints where a parallel `count(*)`
/// over an unbounded range is too expensive and offset pagination is
/// unstable under inserts. The cursor is an opaque server-issued token —
/// clients pass `?cursor=...` on the next request to fetch the page that
/// follows. `next_cursor` is `None` when there is nothing past the
/// returned items.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct CursorPage<T>
where
    T: ToSchema,
{
    pub items: Vec<T>,
    /// Opaque token for the next page; `None` when exhausted.
    pub next_cursor: Option<String>,
}

impl<T> CursorPage<T>
where
    T: ToSchema,
{
    pub fn new(items: Vec<T>, next_cursor: Option<String>) -> Self {
        Self { items, next_cursor }
    }
}

impl<T> RedactInPlace for CursorPage<T>
where
    T: ToSchema + RedactInPlace,
{
    fn redact_in_place(&mut self) {
        for item in &mut self.items {
            item.redact_in_place();
        }
    }
}

pub type CursorPageOfPublicIncident = CursorPage<PublicIncident>;
