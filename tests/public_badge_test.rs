//! Integration tests for the public SVG status badge endpoint.
//!
//! Covers:
//!   * `GET /api/public/v1/badge.svg` returns 200 image/svg+xml with the
//!     overall page state in the badge text.
//!   * `?component={id}` returns the per-component status; unknown id
//!     returns 404 with the public error envelope (no internal detail).
//!   * `?style=` other than `flat` returns 400.
//!   * Service Unavailable propagates as 503 without leaking response shape.

mod common;

use std::sync::Arc;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use chrono::Utc;
use tower::ServiceExt;
use uuid::Uuid;

use common::{UnavailablePublicSource, build_test_app_with_public_source};
use uptimepage::domain::{
    ComponentHistoryResponse, DayState, OverallState, OverallStatus, PageRef, PublicComponent,
    PublicComponentGroup, PublicComponentStatus, PublicIncident, PublicMaintenanceList,
    PublicStatusPage,
};
use uptimepage::error::public::PublicAppError;
use uptimepage::pagination::CursorPage;
use uptimepage::public_status::{IncidentListQuery, PublicSource, source::FeedLinks};

fn known_component_id() -> Uuid {
    Uuid::parse_str("00000000-0000-0000-0000-000000000c01").unwrap()
}

/// Source publishing one degraded component under "API".
struct BadgeSource;

#[async_trait]
impl PublicSource for BadgeSource {
    async fn page(&self, _page: PageRef) -> Result<Arc<PublicStatusPage>, PublicAppError> {
        let component = PublicComponent {
            id: known_component_id(),
            name: "API".into(),
            description: None,
            current_status: PublicComponentStatus::Degraded,
            history: vec![DayState::Operational; 90],
            uptime_pct: Some(99.9),
            detail_url: None,
        };
        Ok(Arc::new(PublicStatusPage {
            overall: OverallStatus {
                state: OverallState::MinorDisruption,
                label: "Minor Disruption".into(),
            },
            generated_at: Utc::now(),
            site_name: "uptimepage".into(),
            groups: vec![PublicComponentGroup {
                name: Some("Edge".into()),
                components: vec![component],
            }],
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
        unimplemented!()
    }
    async fn list_incidents(
        &self,
        _page: PageRef,
        _q: IncidentListQuery,
    ) -> Result<CursorPage<PublicIncident>, PublicAppError> {
        unimplemented!()
    }
    async fn incident_by_id(
        &self,
        _page: PageRef,
        _id: Uuid,
    ) -> Result<PublicIncident, PublicAppError> {
        unimplemented!()
    }
    async fn maintenance(&self, _page: PageRef) -> Result<PublicMaintenanceList, PublicAppError> {
        unimplemented!()
    }
    async fn incidents_rss(
        &self,
        _page: PageRef,
        _links: FeedLinks<'_>,
    ) -> Result<String, PublicAppError> {
        unimplemented!()
    }
}

async fn read_body(resp: axum::response::Response) -> (StatusCode, String, String) {
    let status = resp.status();
    let content_type = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024)
        .await
        .expect("body");
    let body = String::from_utf8(bytes.to_vec()).expect("utf8");
    (status, content_type, body)
}

#[tokio::test]
async fn overall_badge_returns_svg_with_state_label() {
    let app = build_test_app_with_public_source(|_| {}, Arc::new(BadgeSource));
    let req = Request::builder()
        .uri("/api/public/v1/badge.svg")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let (status, ct, body) = read_body(resp).await;
    assert_eq!(status, StatusCode::OK);
    assert!(ct.starts_with("image/svg+xml"), "content-type {ct}");
    assert!(body.starts_with("<svg "));
    assert!(body.contains("uptimepage"));
    assert!(body.contains("minor disruption"));
}

#[tokio::test]
async fn component_badge_returns_per_component_status() {
    let app = build_test_app_with_public_source(|_| {}, Arc::new(BadgeSource));
    let uri = format!(
        "/api/public/v1/badge.svg?component={}",
        known_component_id()
    );
    let req = Request::builder().uri(uri).body(Body::empty()).unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let (status, _, body) = read_body(resp).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("API"));
    assert!(body.contains("degraded"));
}

#[tokio::test]
async fn unknown_component_returns_404_public_envelope() {
    let app = build_test_app_with_public_source(|_| {}, Arc::new(BadgeSource));
    let req = Request::builder()
        .uri("/api/public/v1/badge.svg?component=11111111-1111-1111-1111-111111111111")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let (status, ct, body) = read_body(resp).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(ct.starts_with("application/json"));
    let parsed: serde_json::Value = serde_json::from_str(&body).expect("json envelope");
    assert_eq!(parsed["error"]["code"], "NOT_FOUND");
}

#[tokio::test]
async fn explicit_flat_style_is_accepted() {
    let app = build_test_app_with_public_source(|_| {}, Arc::new(BadgeSource));
    let req = Request::builder()
        .uri("/api/public/v1/badge.svg?style=flat")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let (status, ct, _) = read_body(resp).await;
    assert_eq!(status, StatusCode::OK);
    assert!(ct.starts_with("image/svg+xml"));
}

#[tokio::test]
async fn unsupported_style_returns_400() {
    let app = build_test_app_with_public_source(|_| {}, Arc::new(BadgeSource));
    let req = Request::builder()
        .uri("/api/public/v1/badge.svg?style=plastic")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let (status, _, body) = read_body(resp).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let parsed: serde_json::Value = serde_json::from_str(&body).expect("json envelope");
    assert_eq!(parsed["error"]["code"], "BAD_REQUEST");
}

#[tokio::test]
async fn unavailable_upstream_returns_503() {
    let app = build_test_app_with_public_source(|_| {}, Arc::new(UnavailablePublicSource));
    let req = Request::builder()
        .uri("/api/public/v1/badge.svg")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let (status, _, body) = read_body(resp).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    let parsed: serde_json::Value = serde_json::from_str(&body).expect("json envelope");
    assert_eq!(parsed["error"]["code"], "STATUS_DATA_UNAVAILABLE");
}
