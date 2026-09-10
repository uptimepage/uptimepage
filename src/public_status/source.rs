//! Trait + production impl for the data layer behind the public status API.
//!
//! The trait shape isolates handlers from the concrete storage backend so
//! tests can substitute a deterministic fake without spinning up Postgres or
//! ClickHouse, and so the handler code never reaches across into private
//! storage types.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Context;
use async_trait::async_trait;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use sqlx::{FromRow, PgPool};
use uuid::Uuid;

use crate::api::cursor::IncidentCursor;
use crate::api::page::CursorPage;
use crate::api::public_error::PublicAppError;
use crate::domain::{
    ComponentHistoryResponse, IncidentSeverity, IncidentStatusPhase, OrgId, PageRef,
    PublicIncident, PublicIncidentUpdate, PublicMaintenanceList, PublicStatusPage,
};

use super::aggregator::OrgAggregator;
use super::auto_incident_title;
use super::cache::{HistoryIncidentMarker, PageCache, PageCacheError, PageData};
use super::xml::xml_escape;

#[derive(Debug, Clone, Copy)]
pub struct IncidentListQuery {
    pub limit: u32,
    pub cursor: Option<IncidentCursor>,
    pub ongoing_only: bool,
}

impl Default for IncidentListQuery {
    fn default() -> Self {
        Self {
            limit: 25,
            cursor: None,
            ongoing_only: false,
        }
    }
}

/// Public-status data layer, scoped to a single page. Every method takes a
/// resolved [`PageRef`] (page id + org) — there is no implicit default, so the
/// compiler refuses a handler that forgot which page is being served, which is
/// what keeps the cache from serving one page's data under another's slot.
#[async_trait]
pub trait PublicSource: Send + Sync {
    /// JSON wire shape served by the API. The HTML route uses
    /// `page_with_markers` to get this plus popover data atomically.
    async fn page(&self, page: PageRef) -> Result<Arc<PublicStatusPage>, PublicAppError>;

    /// Page + 90-day popover markers from one atomic snapshot. Default returns
    /// empty markers so backends without popover data (tests, noop self-host)
    /// don't have to think about them; the production source overrides to fetch
    /// both halves from one cache slot.
    async fn page_with_markers(
        &self,
        page: PageRef,
    ) -> Result<(Arc<PublicStatusPage>, Arc<Vec<HistoryIncidentMarker>>), PublicAppError> {
        let p = self.page(page).await?;
        Ok((p, Arc::new(Vec::new())))
    }

    /// Answered from the same cached snapshot the render uses.
    async fn hide_from_search(&self, _page: PageRef) -> bool {
        false
    }

    async fn component_history(
        &self,
        page: PageRef,
        id: Uuid,
        days: u32,
    ) -> Result<ComponentHistoryResponse, PublicAppError>;
    async fn list_incidents(
        &self,
        page: PageRef,
        q: IncidentListQuery,
    ) -> Result<CursorPage<PublicIncident>, PublicAppError>;
    async fn incident_by_id(
        &self,
        page: PageRef,
        id: Uuid,
    ) -> Result<PublicIncident, PublicAppError>;
    async fn maintenance(&self, page: PageRef) -> Result<PublicMaintenanceList, PublicAppError>;
    async fn incidents_rss(
        &self,
        page: PageRef,
        links: FeedLinks<'_>,
    ) -> Result<String, PublicAppError>;

    /// Drop the cached snapshot for a page. The settings handler calls this on
    /// a branding/enable/component edit so a stale page can't outlive its TTL.
    /// Default no-op: backends without a cache have nothing to drop.
    async fn invalidate(&self, _page: crate::domain::StatusPageId) {}
}

pub struct OrgPublicSource {
    aggregator: Arc<OrgAggregator>,
    cache: PageCache,
    pg: PgPool,
    rss_lookback_days: u32,
    rss_max_items: u32,
    site_name: String,
}

impl OrgPublicSource {
    pub fn new(
        aggregator: Arc<OrgAggregator>,
        cache: PageCache,
        pg: PgPool,
        site_name: impl Into<String>,
    ) -> Self {
        Self {
            aggregator,
            cache,
            pg,
            rss_lookback_days: 90,
            rss_max_items: 50,
            site_name: site_name.into(),
        }
    }
}

impl OrgPublicSource {
    /// Shared cache hit; both halves come from one atomic snapshot.
    async fn cached(&self, page: PageRef) -> Result<Arc<PageData>, PublicAppError> {
        let agg = self.aggregator.clone();
        let res = self
            .cache
            .get_or_compute_data(page.page, move || async move {
                agg.build(page.page, page.org).await
            })
            .await;
        match res {
            Ok(data) => Ok(data),
            Err(PageCacheError::Unavailable) => Err(PublicAppError::Unavailable),
        }
    }
}

#[async_trait]
impl PublicSource for OrgPublicSource {
    async fn page(&self, page: PageRef) -> Result<Arc<PublicStatusPage>, PublicAppError> {
        Ok(self.cached(page).await?.page.clone())
    }

    async fn page_with_markers(
        &self,
        page: PageRef,
    ) -> Result<(Arc<PublicStatusPage>, Arc<Vec<HistoryIncidentMarker>>), PublicAppError> {
        let data = self.cached(page).await?;
        Ok((data.page.clone(), data.history_markers.clone()))
    }

    async fn hide_from_search(&self, page: PageRef) -> bool {
        // An unreadable snapshot reads as hidden.
        self.cached(page)
            .await
            .map_or(true, |data| data.hide_from_search)
    }

    async fn component_history(
        &self,
        page: PageRef,
        id: Uuid,
        days: u32,
    ) -> Result<ComponentHistoryResponse, PublicAppError> {
        if !(1..=365).contains(&days) {
            return Err(PublicAppError::InvalidDays);
        }
        self.aggregator
            .component_history(page.page, page.org, id, days)
            .await
            .map_err(|_| PublicAppError::NotFound)
    }

    async fn list_incidents(
        &self,
        page: PageRef,
        q: IncidentListQuery,
    ) -> Result<CursorPage<PublicIncident>, PublicAppError> {
        // The page's component set + per-page names, reused from the cached
        // page snapshot. An incident is on this page iff its target is one of
        // these components.
        let names = self.cached(page).await?.component_names.clone();
        if names.is_empty() {
            return Ok(CursorPage::new(Vec::new(), None));
        }
        let component_ids: Vec<Uuid> = names.keys().copied().collect();

        let since = Utc::now() - ChronoDuration::days(self.rss_lookback_days as i64);
        let limit = q.limit.clamp(1, 100) as i64;
        let ongoing_only = q.ongoing_only;
        let fetch = limit + 1;
        let (cursor_ts, cursor_id) = match q.cursor {
            Some(c) => (Some(c.started_at), Some(c.id)),
            None => (None, None),
        };

        let rows: Vec<IncidentRow> = sqlx::query_as::<_, IncidentRow>(
            r#"SELECT i.id, i.target_id,
                      i.started_at, i.ended_at, i.severity, i.status_at_start,
                      i.public_title, i.public_description
               FROM incidents i
               WHERE i.org_id = $5
                 AND i.target_id = ANY($7)
                 AND i.visibility = 'public'
                 AND i.started_at >= $1
                 AND ($2 = false OR i.ended_at IS NULL)
                 AND (
                     $3::timestamptz IS NULL
                     OR (i.started_at, i.id) < ($3::timestamptz, $4::uuid)
                 )
               ORDER BY i.started_at DESC, i.id DESC
               LIMIT $6"#,
        )
        .bind(since)
        .bind(ongoing_only)
        .bind(cursor_ts)
        .bind(cursor_id)
        .bind(page.org.0)
        .bind(fetch)
        .bind(&component_ids)
        .fetch_all(&self.pg)
        .await
        .context("public list incidents")
        .map_err(PublicAppError::Internal)?;

        let has_more = rows.len() as i64 > limit;
        let mut kept = rows;
        if has_more {
            kept.truncate(limit as usize);
        }
        let next_cursor = if has_more {
            kept.last().map(|r| {
                IncidentCursor {
                    started_at: r.started_at,
                    id: r.id,
                }
                .encode()
            })
        } else {
            None
        };

        let incidents = self.hydrate(page.org, kept, &names).await?;
        Ok(CursorPage::new(incidents, next_cursor))
    }

    async fn incident_by_id(
        &self,
        page: PageRef,
        id: Uuid,
    ) -> Result<PublicIncident, PublicAppError> {
        let names = self.cached(page).await?.component_names.clone();
        let component_ids: Vec<Uuid> = names.keys().copied().collect();
        let row: Option<IncidentRow> = sqlx::query_as::<_, IncidentRow>(
            r#"SELECT i.id, i.target_id,
                      i.started_at, i.ended_at, i.severity, i.status_at_start,
                      i.public_title, i.public_description
               FROM incidents i
               WHERE i.id = $1
                 AND i.org_id = $2
                 AND i.target_id = ANY($3)
                 AND i.visibility = 'public'"#,
        )
        .bind(id)
        .bind(page.org.0)
        .bind(&component_ids)
        .fetch_optional(&self.pg)
        .await
        .context("public get incident")
        .map_err(PublicAppError::Internal)?;

        let row = row.ok_or(PublicAppError::NotFound)?;
        let mut hydrated = self.hydrate(page.org, vec![row], &names).await?;
        let mut incident = hydrated.pop().ok_or(PublicAppError::NotFound)?;
        incident.postmortem = self.published_postmortem(page.org, id).await?;
        Ok(incident)
    }

    async fn maintenance(&self, page: PageRef) -> Result<PublicMaintenanceList, PublicAppError> {
        let p = self.page(page).await?;
        Ok(PublicMaintenanceList {
            active: p.active_maintenance.clone(),
            upcoming: p.upcoming_maintenance.clone(),
        })
    }

    async fn incidents_rss(
        &self,
        page: PageRef,
        links: FeedLinks<'_>,
    ) -> Result<String, PublicAppError> {
        let q = IncidentListQuery {
            limit: self.rss_max_items,
            cursor: None,
            ongoing_only: false,
        };
        let listed = self.list_incidents(page, q).await?;
        Ok(build_rss(&self.site_name, links, &listed.items))
    }

    async fn invalidate(&self, page: crate::domain::StatusPageId) {
        self.cache.invalidate(page).await;
    }
}

impl OrgPublicSource {
    async fn hydrate(
        &self,
        org: OrgId,
        rows: Vec<IncidentRow>,
        names: &HashMap<Uuid, String>,
    ) -> Result<Vec<PublicIncident>, PublicAppError> {
        if rows.is_empty() {
            return Ok(Vec::new());
        }
        let ids: Vec<Uuid> = rows.iter().map(|r| r.id).collect();
        let updates: Vec<UpdateRow> = sqlx::query_as::<_, UpdateRow>(
            r#"SELECT incident_id, posted_at, phase, message
               FROM incident_updates
               WHERE incident_id = ANY($1) AND org_id = $2
               ORDER BY incident_id, posted_at ASC"#,
        )
        .bind(&ids)
        .bind(org.0)
        .fetch_all(&self.pg)
        .await
        .context("public list incident updates")
        .map_err(PublicAppError::Internal)?;

        Ok(rows
            .into_iter()
            .map(|r| {
                let component_name = names.get(&r.target_id).cloned().unwrap_or_default();
                let mut my_updates: Vec<PublicIncidentUpdate> = updates
                    .iter()
                    .filter(|u| u.incident_id == r.id)
                    .map(|u| PublicIncidentUpdate {
                        posted_at: u.posted_at,
                        phase: IncidentStatusPhase::from_db_str(&u.phase),
                        message: u.message.clone(),
                    })
                    .collect();
                my_updates.sort_by_key(|u| u.posted_at);
                let status_phase = my_updates
                    .last()
                    .map(|u| u.phase)
                    .unwrap_or(IncidentStatusPhase::Investigating);
                let title = r
                    .public_title
                    .clone()
                    .unwrap_or_else(|| auto_incident_title(&component_name, &r.status_at_start));
                PublicIncident {
                    id: r.id,
                    component_id: r.target_id,
                    component_name,
                    title,
                    started_at: r.started_at,
                    ended_at: r.ended_at,
                    severity: IncidentSeverity::from_db_str(&r.severity),
                    status_phase,
                    updates: my_updates,
                    postmortem: None,
                }
            })
            .collect())
    }

    /// Published postmortem for one incident, if the operator has published it.
    /// Internal fields (the action-item owner) are dropped here.
    async fn published_postmortem(
        &self,
        org: OrgId,
        incident_id: Uuid,
    ) -> Result<Option<crate::domain::PublicPostmortem>, PublicAppError> {
        let row: Option<PostmortemRow> = sqlx::query_as::<_, PostmortemRow>(
            r#"SELECT summary, root_cause, impact, action_items, published_at
               FROM incident_postmortems
               WHERE incident_id = $1 AND org_id = $2 AND published_at IS NOT NULL"#,
        )
        .bind(incident_id)
        .bind(org.0)
        .fetch_optional(&self.pg)
        .await
        .context("public get postmortem")
        .map_err(PublicAppError::Internal)?;
        Ok(row.map(|r| {
            #[derive(serde::Deserialize)]
            struct RawItem {
                text: String,
                #[serde(default)]
                done: bool,
            }
            let action_items = serde_json::from_value::<Vec<RawItem>>(r.action_items)
                .unwrap_or_default()
                .into_iter()
                .map(|i| crate::domain::PublicActionItem {
                    text: i.text,
                    done: i.done,
                })
                .collect();
            crate::domain::PublicPostmortem {
                summary: r.summary,
                root_cause: r.root_cause,
                impact: r.impact,
                action_items,
                published_at: r.published_at,
            }
        }))
    }
}

#[derive(FromRow)]
struct PostmortemRow {
    summary: Option<String>,
    root_cause: Option<String>,
    impact: Option<String>,
    action_items: serde_json::Value,
    published_at: DateTime<Utc>,
}

#[derive(FromRow)]
struct IncidentRow {
    id: Uuid,
    target_id: Uuid,
    started_at: DateTime<Utc>,
    ended_at: Option<DateTime<Utc>>,
    severity: String,
    status_at_start: String,
    public_title: Option<String>,
    #[allow(dead_code)]
    public_description: Option<String>,
}

#[derive(FromRow)]
struct UpdateRow {
    incident_id: Uuid,
    posted_at: DateTime<Utc>,
    phase: String,
    message: String,
}

/// Where a feed's links point. The channel link is the page a reader lands on
/// from the feed title, which is not the origin on a deploy that serves the
/// page under a path.
#[derive(Debug, Clone, Copy)]
pub struct FeedLinks<'a> {
    pub page: &'a str,
    pub origin: &'a str,
}

/// Minimal RSS 2.0 builder — keeps us free of an extra crate just to emit
/// a few dozen lines of XML. Items are the most recent public incidents.
pub fn build_rss(site_name: &str, links: FeedLinks<'_>, items: &[PublicIncident]) -> String {
    let base_url = links.origin.trim_end_matches('/');
    let now = Utc::now().to_rfc2822();
    let mut out = String::with_capacity(512 + items.len() * 256);
    out.push_str(r#"<?xml version="1.0" encoding="UTF-8"?>"#);
    out.push_str("\n<rss version=\"2.0\"><channel>");
    out.push_str(&format!(
        "<title>{}</title><link>{}</link><description>Operational status</description><lastBuildDate>{}</lastBuildDate>",
        xml_escape(&format!("{site_name} Status Incidents")),
        xml_escape(links.page.trim_end_matches('/')),
        now,
    ));
    for i in items {
        let pub_date = i
            .updates
            .last()
            .map(|u| u.posted_at)
            .unwrap_or(i.started_at)
            .to_rfc2822();
        let body: String = i
            .updates
            .iter()
            .map(|u| format!("[{}] {}", phase_label(u.phase), u.message))
            .collect::<Vec<_>>()
            .join(" \n");
        out.push_str("<item>");
        out.push_str(&format!("<title>{}</title>", xml_escape(&i.title)));
        out.push_str(&format!("<guid isPermaLink=\"false\">{}</guid>", i.id));
        out.push_str(&format!(
            "<link>{}/status/incidents/{}</link>",
            xml_escape(base_url),
            i.id
        ));
        out.push_str(&format!("<pubDate>{pub_date}</pubDate>"));
        out.push_str(&format!("<description>{}</description>", xml_escape(&body)));
        out.push_str("</item>");
    }
    out.push_str("</channel></rss>");
    out
}

fn phase_label(p: IncidentStatusPhase) -> &'static str {
    match p {
        IncidentStatusPhase::Investigating => "investigating",
        IncidentStatusPhase::Identified => "identified",
        IncidentStatusPhase::Monitoring => "monitoring",
        IncidentStatusPhase::Resolved => "resolved",
        IncidentStatusPhase::Postmortem => "postmortem",
    }
}

/// A `PublicSource` that returns an empty page and `NotFound` for everything
/// else. Useful as a placeholder in test rigs that don't exercise the public
/// surface, and as a safe fallback for environments without ClickHouse.
pub struct NoopPublicSource {
    site_name: String,
}

impl NoopPublicSource {
    pub fn new(site_name: impl Into<String>) -> Self {
        Self {
            site_name: site_name.into(),
        }
    }
}

impl Default for NoopPublicSource {
    fn default() -> Self {
        Self::new("uptimepage")
    }
}

#[async_trait]
impl PublicSource for NoopPublicSource {
    async fn page(&self, _page: PageRef) -> Result<Arc<PublicStatusPage>, PublicAppError> {
        Ok(Arc::new(PublicStatusPage {
            overall: crate::domain::OverallStatus {
                state: crate::domain::OverallState::Operational,
                label: "All Systems Operational".into(),
            },
            generated_at: Utc::now(),
            site_name: self.site_name.clone(),
            groups: Vec::new(),
            active_incidents: Vec::new(),
            recent_incidents: Vec::new(),
            recent_incidents_has_more: false,
            active_maintenance: Vec::new(),
            upcoming_maintenance: Vec::new(),
        }))
    }

    async fn component_history(
        &self,
        _page: PageRef,
        _id: Uuid,
        _days: u32,
    ) -> Result<ComponentHistoryResponse, PublicAppError> {
        Err(PublicAppError::NotFound)
    }

    async fn list_incidents(
        &self,
        _page: PageRef,
        _q: IncidentListQuery,
    ) -> Result<CursorPage<PublicIncident>, PublicAppError> {
        Ok(CursorPage::new(Vec::new(), None))
    }

    async fn incident_by_id(
        &self,
        _page: PageRef,
        _id: Uuid,
    ) -> Result<PublicIncident, PublicAppError> {
        Err(PublicAppError::NotFound)
    }

    async fn maintenance(&self, _page: PageRef) -> Result<PublicMaintenanceList, PublicAppError> {
        Ok(PublicMaintenanceList {
            active: Vec::new(),
            upcoming: Vec::new(),
        })
    }

    async fn incidents_rss(
        &self,
        _page: PageRef,
        links: FeedLinks<'_>,
    ) -> Result<String, PublicAppError> {
        Ok(build_rss(&self.site_name, links, &[]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tenant host serves the page at its root, so both links share an origin.
    fn links(origin: &str) -> FeedLinks<'_> {
        FeedLinks {
            page: origin,
            origin,
        }
    }

    #[test]
    fn rss_skeleton_well_formed_with_no_items() {
        let xml = build_rss("Site", links("https://example.com"), &[]);
        assert!(xml.starts_with("<?xml"));
        assert!(xml.contains("<rss version=\"2.0\""));
        assert!(xml.contains("<channel>"));
        assert!(xml.contains("</channel></rss>"));
        assert!(xml.contains("Site Status Incidents"));
    }

    fn sample_incident(id: u128, updates: Vec<PublicIncidentUpdate>) -> PublicIncident {
        let started = DateTime::parse_from_rfc3339("2026-05-22T10:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        PublicIncident {
            id: Uuid::from_u128(id),
            component_id: Uuid::nil(),
            component_name: "Edge".into(),
            title: "Edge proxy 5xx".into(),
            started_at: started,
            ended_at: None,
            severity: crate::domain::IncidentSeverity::Major,
            status_phase: IncidentStatusPhase::Investigating,
            updates,
            postmortem: None,
        }
    }

    fn update(minutes: i64, phase: IncidentStatusPhase, message: &str) -> PublicIncidentUpdate {
        let posted = DateTime::parse_from_rfc3339("2026-05-22T10:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
            + chrono::Duration::minutes(minutes);
        PublicIncidentUpdate {
            posted_at: posted,
            phase,
            message: message.into(),
        }
    }

    #[test]
    fn item_link_is_the_incident_permalink() {
        let inc = sample_incident(0xc01, vec![]);
        let xml = build_rss(
            "Site",
            links("https://acme.example.com"),
            std::slice::from_ref(&inc),
        );
        assert!(
            xml.contains(&format!(
                "<link>https://acme.example.com/status/incidents/{}</link>",
                inc.id
            )),
            "{xml}"
        );
    }

    #[test]
    fn a_trailing_slash_on_the_origin_does_not_double_up() {
        let inc = sample_incident(0xc01, vec![]);
        let xml = build_rss(
            "Site",
            links("https://acme.example.com/"),
            std::slice::from_ref(&inc),
        );
        assert!(!xml.contains("com//status"), "{xml}");
        assert!(
            xml.contains("<link>https://acme.example.com</link>"),
            "{xml}"
        );
    }

    #[test]
    fn markup_in_customer_text_is_escaped() {
        let mut inc = sample_incident(
            0xc01,
            vec![update(5, IncidentStatusPhase::Identified, "a < b & c")],
        );
        inc.title = "<script>alert(1)</script>".into();
        let xml = build_rss(
            "A & B",
            links("https://acme.example.com"),
            std::slice::from_ref(&inc),
        );
        assert!(!xml.contains("<script>"), "{xml}");
        assert!(xml.contains("&lt;script&gt;"), "{xml}");
        assert!(xml.contains("a &lt; b &amp; c"), "{xml}");
        assert!(xml.contains("A &amp; B Status Incidents"), "{xml}");
    }

    #[test]
    fn an_incident_with_no_updates_dates_from_its_start() {
        let inc = sample_incident(0xc01, vec![]);
        let xml = build_rss(
            "Site",
            links("https://acme.example.com"),
            std::slice::from_ref(&inc),
        );
        assert!(xml.contains(&format!(
            "<pubDate>{}</pubDate>",
            inc.started_at.to_rfc2822()
        )));
        assert!(xml.contains("<description></description>"), "{xml}");
    }

    #[test]
    fn the_channel_link_is_the_page_not_the_origin() {
        // A path-based deploy serves the dashboard at the origin root, so a
        // reader clicking the feed title there would land on the login screen.
        let inc = sample_incident(0xc01, vec![]);
        let xml = build_rss(
            "Site",
            FeedLinks {
                page: "https://status.example.test/status",
                origin: "https://status.example.test",
            },
            std::slice::from_ref(&inc),
        );
        assert!(
            xml.contains("<link>https://status.example.test/status</link>"),
            "{xml}"
        );
        assert!(
            xml.contains(&format!(
                "<link>https://status.example.test/status/incidents/{}</link>",
                inc.id
            )),
            "{xml}"
        );
    }

    #[test]
    fn updates_are_labelled_by_phase_and_dated_from_the_last_one() {
        let inc = sample_incident(
            0xc01,
            vec![
                update(0, IncidentStatusPhase::Investigating, "looking"),
                update(30, IncidentStatusPhase::Resolved, "fixed"),
            ],
        );
        let xml = build_rss(
            "Site",
            links("https://acme.example.com"),
            std::slice::from_ref(&inc),
        );
        assert!(xml.contains("[investigating] looking"), "{xml}");
        assert!(xml.contains("[resolved] fixed"), "{xml}");
        assert!(xml.contains(&format!(
            "<pubDate>{}</pubDate>",
            inc.updates.last().unwrap().posted_at.to_rfc2822()
        )));
    }
}
