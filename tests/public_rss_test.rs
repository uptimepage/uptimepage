//! RSS 2.0 well-formedness + required-element validation for
//! `/api/public/v1/incidents.rss`.
//!
//! The hand-rolled RSS emitter in `src/public_status/source.rs` is one of the
//! few places where a quietly broken response would still parse on the client
//! side (browsers and feed readers are forgiving) — so we parse it strictly
//! with `quick-xml` and assert the required structural elements per
//! https://www.rssboard.org/rss-specification.

mod common;

use std::sync::Arc;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::{DateTime, Utc};
use quick_xml::Reader;
use quick_xml::events::Event;
use tower::ServiceExt;
use url::Url;
use uuid::Uuid;

use common::build_test_app_with_public_source;
use uptimepage::api::CursorPage;
use uptimepage::api::public_error::PublicAppError;
use uptimepage::domain::{
    ComponentHistoryResponse, IncidentSeverity, IncidentStatusPhase, PageRef, PublicIncident,
    PublicIncidentUpdate, PublicMaintenanceList, PublicStatusPage,
};
use uptimepage::public_status::{
    IncidentListQuery, PublicSource, source::FeedLinks, source::build_rss,
};

const INCIDENT_TITLE: &str = "Edge proxy 5xx spike";
const INCIDENT_BODY: &str = "First report from the edge fleet — investigating.";

fn incident_id() -> Uuid {
    Uuid::parse_str("00000000-0000-0000-0000-000000000c01").unwrap()
}

/// Two-incident source so we exercise both feed structure and per-item
/// invariants (GUID uniqueness, pubDate ordering, etc.).
struct TwoIncidentSource;

#[async_trait]
impl PublicSource for TwoIncidentSource {
    async fn page(&self, _page: PageRef) -> Result<Arc<PublicStatusPage>, PublicAppError> {
        unimplemented!("not exercised by RSS test")
    }
    async fn component_history(
        &self,
        _page: PageRef,
        _id: Uuid,
        _days: u32,
    ) -> Result<ComponentHistoryResponse, PublicAppError> {
        unimplemented!("not exercised by RSS test")
    }
    async fn list_incidents(
        &self,
        _page: PageRef,
        _q: IncidentListQuery,
    ) -> Result<CursorPage<PublicIncident>, PublicAppError> {
        let now = Utc::now();
        let items = vec![
            PublicIncident {
                id: incident_id(),
                component_id: Uuid::nil(),
                component_name: "Edge".into(),
                title: INCIDENT_TITLE.into(),
                started_at: now - chrono::Duration::minutes(30),
                ended_at: None,
                severity: IncidentSeverity::Major,
                status_phase: IncidentStatusPhase::Investigating,
                updates: vec![PublicIncidentUpdate {
                    posted_at: now - chrono::Duration::minutes(5),
                    phase: IncidentStatusPhase::Investigating,
                    message: INCIDENT_BODY.into(),
                }],
                postmortem: None,
            },
            PublicIncident {
                id: Uuid::parse_str("00000000-0000-0000-0000-000000000c02").unwrap(),
                component_id: Uuid::nil(),
                component_name: "Edge".into(),
                title: "Origin TLS renewal".into(),
                started_at: now - chrono::Duration::hours(6),
                ended_at: Some(now - chrono::Duration::hours(5)),
                severity: IncidentSeverity::Minor,
                status_phase: IncidentStatusPhase::Resolved,
                updates: vec![],
                postmortem: None,
            },
        ];
        Ok(CursorPage::new(items, None))
    }
    async fn incident_by_id(
        &self,
        _page: PageRef,
        _id: Uuid,
    ) -> Result<PublicIncident, PublicAppError> {
        unimplemented!("not exercised by RSS test")
    }
    async fn maintenance(&self, _page: PageRef) -> Result<PublicMaintenanceList, PublicAppError> {
        unimplemented!("not exercised by RSS test")
    }
    async fn incidents_rss(
        &self,
        page: PageRef,
        links: FeedLinks<'_>,
    ) -> Result<String, PublicAppError> {
        let items = self
            .list_incidents(page, IncidentListQuery::default())
            .await?
            .items;
        Ok(build_rss("uptimepage", links, &items))
    }
}

/// Same feed, from a page the operator asked to keep out of search results.
struct HiddenPageSource(TwoIncidentSource);

#[async_trait]
impl PublicSource for HiddenPageSource {
    async fn page(&self, page: PageRef) -> Result<Arc<PublicStatusPage>, PublicAppError> {
        self.0.page(page).await
    }
    async fn hide_from_search(&self, _page: PageRef) -> bool {
        true
    }
    async fn component_history(
        &self,
        page: PageRef,
        id: Uuid,
        days: u32,
    ) -> Result<ComponentHistoryResponse, PublicAppError> {
        self.0.component_history(page, id, days).await
    }
    async fn list_incidents(
        &self,
        page: PageRef,
        q: IncidentListQuery,
    ) -> Result<CursorPage<PublicIncident>, PublicAppError> {
        self.0.list_incidents(page, q).await
    }
    async fn incident_by_id(
        &self,
        page: PageRef,
        id: Uuid,
    ) -> Result<PublicIncident, PublicAppError> {
        self.0.incident_by_id(page, id).await
    }
    async fn maintenance(&self, page: PageRef) -> Result<PublicMaintenanceList, PublicAppError> {
        self.0.maintenance(page).await
    }
    async fn incidents_rss(
        &self,
        page: PageRef,
        links: FeedLinks<'_>,
    ) -> Result<String, PublicAppError> {
        self.0.incidents_rss(page, links).await
    }
}

const BASE_DOMAIN: &str = "example.test";
const PUBLIC_BASE_URL: &str = "https://status.example.test";

async fn fetch_rss() -> String {
    fetch_rss_from(None).await
}

/// Fetch the feed with an explicit `Host`, on a deployment that owns
/// `{slug}.example.test`.
async fn fetch_rss_from(host: Option<&str>) -> String {
    let app = build_test_app_with_public_source(
        |cfg| {
            cfg.public_status.base_domain = BASE_DOMAIN.into();
            cfg.auth.public_base_url = PUBLIC_BASE_URL.into();
        },
        Arc::new(TwoIncidentSource),
    );
    let mut req = Request::get("/api/public/v1/incidents.rss");
    if let Some(h) = host {
        req = req.header("host", h);
    }
    let resp = app.oneshot(req.body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), 8 << 20)
        .await
        .unwrap();
    String::from_utf8(bytes.to_vec()).expect("rss feed must be utf-8")
}

/// Walks the RSS document with `quick-xml` and returns the structure we
/// validate against. Strict end-name checks are on by default — malformed XML
/// aborts the test.
///
/// Each `<item>` block is captured as an ordered list of `(tag, text)` pairs
/// so per-element assertions (URI shape, RFC-822 pubDate, GUID uniqueness)
/// can inspect actual content, not just presence.
struct ParsedRss {
    rss_version: String,
    channel_elements: Vec<String>,
    item_blocks: Vec<Vec<(String, String)>>,
}

fn parse(xml: &str) -> ParsedRss {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut rss_version = String::new();
    let mut channel_elements = Vec::new();
    let mut item_blocks: Vec<Vec<(String, String)>> = Vec::new();
    let mut current_item: Option<Vec<(String, String)>> = None;
    let mut depth_channel = 0u32;
    let mut current_tag: Option<String> = None;
    let mut current_text = String::new();
    loop {
        match reader.read_event_into(&mut buf).expect("strict XML parse") {
            Event::Eof => break,
            Event::Start(e) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).into_owned();
                if name == "rss" {
                    for attr in e.attributes().with_checks(false).flatten() {
                        if attr.key.as_ref() == b"version" {
                            rss_version = String::from_utf8_lossy(&attr.value).into_owned();
                        }
                    }
                } else if name == "channel" {
                    depth_channel += 1;
                } else if name == "item" {
                    current_item = Some(Vec::new());
                } else {
                    current_tag = Some(name);
                    current_text.clear();
                }
            }
            Event::Text(e) => {
                if current_tag.is_some()
                    && let Ok(s) = e.decode()
                {
                    current_text.push_str(&s);
                }
            }
            Event::End(e) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).into_owned();
                if name == "channel" {
                    depth_channel = depth_channel.saturating_sub(1);
                } else if name == "item"
                    && let Some(block) = current_item.take()
                {
                    item_blocks.push(block);
                } else if current_tag.as_deref() == Some(name.as_str()) {
                    let text = std::mem::take(&mut current_text);
                    if let Some(item) = current_item.as_mut() {
                        item.push((name.clone(), text));
                    } else if depth_channel > 0 {
                        channel_elements.push(name.clone());
                    }
                    current_tag = None;
                }
            }
            _ => {}
        }
        buf.clear();
    }
    ParsedRss {
        rss_version,
        channel_elements,
        item_blocks,
    }
}

fn item_text<'a>(block: &'a [(String, String)], tag: &str) -> Option<&'a str> {
    block
        .iter()
        .find(|(t, _)| t == tag)
        .map(|(_, v)| v.as_str())
}

#[tokio::test]
async fn rss_feed_parses_strictly_as_xml() {
    let xml = fetch_rss().await;
    // quick-xml.read_event panics inside `parse` if the document is not
    // well-formed; this assertion is mostly a no-op once we get here, but
    // double-checks we received non-empty bytes.
    assert!(xml.contains("<?xml"));
    let _ = parse(&xml);
}

#[tokio::test]
async fn rss_root_declares_version_2_0() {
    let xml = fetch_rss().await;
    let parsed = parse(&xml);
    assert_eq!(parsed.rss_version, "2.0", "rss version must be 2.0");
}

#[tokio::test]
async fn rss_channel_has_required_children() {
    // RSS 2.0 §"Required channel elements": title, link, description.
    // We also emit lastBuildDate (recommended) — assert present so a future
    // refactor doesn't silently drop it.
    let xml = fetch_rss().await;
    let parsed = parse(&xml);
    for required in ["title", "link", "description", "lastBuildDate"] {
        assert!(
            parsed.channel_elements.iter().any(|e| e == required),
            "channel missing <{required}>: saw {:?}",
            parsed.channel_elements
        );
    }
}

#[tokio::test]
async fn rss_item_has_required_children() {
    // RSS 2.0 says an item must contain *at least* title OR description, plus
    // a guid for stability. We emit all five (title, link, guid, pubDate,
    // description) and assert each so the feed renders sensibly in readers.
    let xml = fetch_rss().await;
    let parsed = parse(&xml);
    assert!(
        !parsed.item_blocks.is_empty(),
        "feed must contain at least one <item>"
    );
    for block in &parsed.item_blocks {
        for required in ["title", "link", "guid", "pubDate", "description"] {
            assert!(
                block.iter().any(|(t, _)| t == required),
                "<item> missing <{required}>: saw {block:?}"
            );
        }
    }
}

#[tokio::test]
async fn rss_item_pubdate_parses_as_rfc822() {
    // Feed readers reject items whose `<pubDate>` isn't a valid RFC-822 date.
    // The hand-rolled builder uses `DateTime::to_rfc2822()`; if a refactor
    // ever swaps that for RFC-3339, every feed reader breaks silently.
    let xml = fetch_rss().await;
    let parsed = parse(&xml);
    for block in &parsed.item_blocks {
        let raw = item_text(block, "pubDate").expect("pubDate present");
        DateTime::parse_from_rfc2822(raw).unwrap_or_else(|e| {
            panic!("pubDate '{raw}' is not RFC-822: {e}");
        });
    }
}

#[tokio::test]
async fn rss_item_link_parses_as_absolute_uri() {
    let xml = fetch_rss().await;
    let parsed = parse(&xml);
    for block in &parsed.item_blocks {
        let raw = item_text(block, "link").expect("link present");
        let u = Url::parse(raw).unwrap_or_else(|e| panic!("link '{raw}' invalid: {e}"));
        assert!(
            u.scheme() == "http" || u.scheme() == "https",
            "link must be http(s): {raw}"
        );
    }
}

#[tokio::test]
async fn rss_links_ignore_the_host_where_every_host_serves_one_page() {
    // Single-tenant: the page is the same whatever the Host says, so a Host
    // header names no better origin than the config — and a forged one would
    // otherwise rewrite links a reader keeps.
    for host in [
        None,
        Some("acme.example.test"),
        Some("evil.test"),
        Some("a.b.example.test"),
    ] {
        let xml = fetch_rss_from(host).await;
        let parsed = parse(&xml);
        for block in &parsed.item_blocks {
            let raw = item_text(block, "link").expect("link present");
            assert!(
                raw.starts_with(&format!("{PUBLIC_BASE_URL}/status/incidents/")),
                "host {host:?} reached a feed link: {raw}"
            );
        }
    }
}

#[tokio::test]
async fn rss_channel_link_is_the_page_not_the_operator_root() {
    // Single-tenant serves the dashboard at `/`, so a reader clicking the feed
    // title there would land on the login screen.
    let xml = fetch_rss().await;
    assert!(
        xml.contains(&format!("<link>{PUBLIC_BASE_URL}/status</link>")),
        "{xml}"
    );
}

#[tokio::test]
async fn rss_links_are_never_the_listen_address() {
    let xml = fetch_rss().await;
    assert!(
        !xml.contains("127.0.0.1") && !xml.contains("0.0.0.0"),
        "feed links must be reachable off-box:\n{xml}"
    );
}

#[tokio::test]
async fn rss_item_guids_are_unique() {
    // RSS 2.0 §"guid" — the value must be unique across the feed.
    let xml = fetch_rss().await;
    let parsed = parse(&xml);
    let mut seen: std::collections::HashSet<&str> = Default::default();
    for block in &parsed.item_blocks {
        let g = item_text(block, "guid").expect("guid present");
        assert!(seen.insert(g), "duplicate guid: {g}");
    }
}

#[tokio::test]
async fn feed_of_a_hidden_page_carries_the_noindex_header() {
    for (source, expected) in [
        (
            Arc::new(HiddenPageSource(TwoIncidentSource)) as Arc<dyn PublicSource>,
            Some("noindex"),
        ),
        (Arc::new(TwoIncidentSource) as Arc<dyn PublicSource>, None),
    ] {
        let app = build_test_app_with_public_source(
            |cfg| {
                cfg.public_status.base_domain = BASE_DOMAIN.into();
                cfg.auth.public_base_url = PUBLIC_BASE_URL.into();
            },
            source,
        );
        let resp = app
            .oneshot(
                Request::get("/api/public/v1/incidents.rss")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers()
                .get("x-robots-tag")
                .map(|v| v.to_str().unwrap()),
            expected
        );
    }
}
