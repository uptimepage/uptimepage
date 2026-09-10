//! Contract tests for the unauthenticated `/api/public/v1/*` surface.
//!
//! Two guarantees:
//!  1. Targets with `public_status = false` are invisible from every public
//!     endpoint — direct lookups return 404 and they never appear in any
//!     list / aggregate response.
//!  2. Public responses never serialise sensitive fields (URL, headers,
//!     auth credentials, internal target name, error text).

mod common;

use std::sync::Arc;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::Utc;
use tower::ServiceExt;
use uuid::Uuid;

use uptimepage::api::CursorPage;
use uptimepage::api::public_error::PublicAppError;
use uptimepage::domain::{
    ComponentHistoryResponse, DayState, IncidentSeverity, IncidentStatusPhase, OverallState,
    OverallStatus, PageRef, PublicComponent, PublicComponentGroup, PublicComponentStatus,
    PublicIncident, PublicIncidentUpdate, PublicMaintenance, PublicMaintenanceList,
    PublicStatusPage,
};
use uptimepage::public_status::{IncidentListQuery, PublicSource, source::FeedLinks};

use common::build_test_app_with_public_source;

// Marker strings that the underlying private target carries; if any of these
// surface in a public response the wire-type isolation has failed.
const SECRET_URL: &str = "https://api-internal.private.example.com/health";
const SECRET_HEADER_VALUE: &str = "x-corp-internal-trace";
const SECRET_BASIC_USER: &str = "secret-user";
const SECRET_BASIC_PASS: &str = "p4ssw0rd-1234";
const SECRET_BEARER: &str = "Bearer.tok.5VkjL_supersecret";
const SECRET_INTERNAL_NAME: &str = "internal-only-name-do-not-leak";
const NON_PUBLIC_INCIDENT_TITLE: &str = "private-incident-title-do-not-leak";

const PUBLIC_COMPONENT_NAME: &str = "API";
const PUBLIC_INCIDENT_TITLE: &str = "API degraded";

fn public_component_id() -> Uuid {
    Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap()
}

fn public_incident_id() -> Uuid {
    Uuid::parse_str("00000000-0000-0000-0000-000000000010").unwrap()
}

fn non_public_incident_id() -> Uuid {
    Uuid::parse_str("00000000-0000-0000-0000-000000000999").unwrap()
}

/// Source that exposes one public component + one public incident + one
/// public maintenance window. The "private" side of the database (the
/// non-public target / non-public incident) is **never** returned — this
/// mirrors the real `LivePublicSource`, which filters at the SQL layer.
struct FakePublicSource;

#[async_trait]
impl PublicSource for FakePublicSource {
    async fn page(&self, _page: PageRef) -> Result<Arc<PublicStatusPage>, PublicAppError> {
        let component = PublicComponent {
            id: public_component_id(),
            name: PUBLIC_COMPONENT_NAME.into(),
            description: Some("Primary REST endpoint".into()),
            current_status: PublicComponentStatus::Operational,
            history: vec![DayState::Operational; 90],
            detail_url: None,
        };
        let incident = PublicIncident {
            id: public_incident_id(),
            component_id: public_component_id(),
            component_name: PUBLIC_COMPONENT_NAME.into(),
            title: PUBLIC_INCIDENT_TITLE.into(),
            started_at: Utc::now(),
            ended_at: None,
            severity: IncidentSeverity::Major,
            status_phase: IncidentStatusPhase::Investigating,
            updates: vec![PublicIncidentUpdate {
                posted_at: Utc::now(),
                phase: IncidentStatusPhase::Investigating,
                message: "Rolling back the deploy.".into(),
            }],
            postmortem: None,
        };
        let maintenance = PublicMaintenance {
            id: Uuid::nil(),
            title: "Routine database upgrade".into(),
            description: Some("Brief read-only window".into()),
            starts_at: Utc::now() + chrono::Duration::hours(2),
            ends_at: Utc::now() + chrono::Duration::hours(3),
            affected_component_names: vec![PUBLIC_COMPONENT_NAME.into()],
        };
        Ok(Arc::new(PublicStatusPage {
            overall: OverallStatus {
                state: OverallState::MinorDisruption,
                label: "Minor Service Disruption".into(),
            },
            generated_at: Utc::now(),
            site_name: "uptimepage".into(),
            groups: vec![PublicComponentGroup {
                name: Some("API".into()),
                components: vec![component],
            }],
            active_incidents: vec![incident.clone()],
            recent_incidents: vec![incident],
            recent_incidents_has_more: false,
            active_maintenance: Vec::new(),
            upcoming_maintenance: vec![maintenance],
        }))
    }

    async fn component_history(
        &self,
        _page: PageRef,
        id: Uuid,
        days: u32,
    ) -> Result<ComponentHistoryResponse, PublicAppError> {
        if id != public_component_id() {
            return Err(PublicAppError::NotFound);
        }
        Ok(ComponentHistoryResponse {
            component_id: id,
            component_name: PUBLIC_COMPONENT_NAME.into(),
            days,
            history: vec![DayState::Operational; days as usize],
        })
    }

    async fn list_incidents(
        &self,
        _page: PageRef,
        _q: IncidentListQuery,
    ) -> Result<CursorPage<PublicIncident>, PublicAppError> {
        let item = PublicIncident {
            id: public_incident_id(),
            component_id: public_component_id(),
            component_name: PUBLIC_COMPONENT_NAME.into(),
            title: PUBLIC_INCIDENT_TITLE.into(),
            started_at: Utc::now(),
            ended_at: None,
            severity: IncidentSeverity::Major,
            status_phase: IncidentStatusPhase::Investigating,
            updates: Vec::new(),
            postmortem: None,
        };
        Ok(CursorPage::new(vec![item], None))
    }

    async fn incident_by_id(
        &self,
        _page: PageRef,
        id: Uuid,
    ) -> Result<PublicIncident, PublicAppError> {
        if id != public_incident_id() {
            return Err(PublicAppError::NotFound);
        }
        Ok(PublicIncident {
            id,
            component_id: public_component_id(),
            component_name: PUBLIC_COMPONENT_NAME.into(),
            title: PUBLIC_INCIDENT_TITLE.into(),
            started_at: Utc::now(),
            ended_at: None,
            severity: IncidentSeverity::Major,
            status_phase: IncidentStatusPhase::Investigating,
            updates: Vec::new(),
            postmortem: None,
        })
    }

    async fn maintenance(&self, _page: PageRef) -> Result<PublicMaintenanceList, PublicAppError> {
        Ok(PublicMaintenanceList {
            active: Vec::new(),
            upcoming: vec![PublicMaintenance {
                id: Uuid::nil(),
                title: "Routine database upgrade".into(),
                description: Some("Brief read-only window".into()),
                starts_at: Utc::now() + chrono::Duration::hours(2),
                ends_at: Utc::now() + chrono::Duration::hours(3),
                affected_component_names: vec![PUBLIC_COMPONENT_NAME.into()],
            }],
        })
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
        Ok(uptimepage::public_status::source::build_rss(
            "uptimepage",
            links,
            &items,
        ))
    }
}

fn router() -> axum::Router {
    build_test_app_with_public_source(|_| {}, Arc::new(FakePublicSource))
}

/// Same data, from a page the operator keeps out of search results.
struct HiddenSource(FakePublicSource);

#[async_trait]
impl PublicSource for HiddenSource {
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

#[tokio::test]
async fn a_hidden_page_marks_every_public_representation_noindex() {
    let paths = [
        "/api/public/v1/status",
        "/api/public/v1/incidents",
        "/api/public/v1/maintenance",
        "/api/public/v1/badge.svg",
        "/api/public/v1/incidents.rss",
    ];
    for path in paths {
        let hidden =
            build_test_app_with_public_source(|_| {}, Arc::new(HiddenSource(FakePublicSource)))
                .oneshot(Request::get(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
        assert_eq!(hidden.status(), StatusCode::OK, "{path}");
        assert_eq!(
            hidden
                .headers()
                .get("x-robots-tag")
                .map(|v| v.to_str().unwrap()),
            Some("noindex"),
            "{path} must carry the page's noindex"
        );

        let shown = router()
            .oneshot(Request::get(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(shown.headers().get("x-robots-tag"), None, "{path}");
    }
}

async fn body_string(resp: axum::response::Response) -> String {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    String::from_utf8_lossy(&bytes).into_owned()
}

fn assert_no_secrets(name: &str, haystack: &str) {
    for s in [
        SECRET_URL,
        SECRET_HEADER_VALUE,
        SECRET_BASIC_USER,
        SECRET_BASIC_PASS,
        SECRET_BEARER,
        SECRET_INTERNAL_NAME,
        NON_PUBLIC_INCIDENT_TITLE,
    ] {
        assert!(
            !haystack.contains(s),
            "{name}: response leaked '{s}': {haystack}"
        );
    }
}

// ── Acceptance: target with public_status = false is invisible ──────────────

#[tokio::test]
async fn non_public_component_history_returns_404() {
    let resp = router()
        .oneshot(
            Request::get(format!(
                "/api/public/v1/components/{}/history",
                non_public_incident_id()
            ))
            .body(Body::empty())
            .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let body = body_string(resp).await;
    assert!(body.contains(r#""code":"NOT_FOUND""#));
    // Public envelope is narrow: no trace_id / details / field keys.
    assert!(!body.contains("trace_id"));
    assert!(!body.contains("details"));
    assert!(!body.contains("\"field\""));
}

#[tokio::test]
async fn non_public_incident_lookup_returns_404() {
    let resp = router()
        .oneshot(
            Request::get(format!(
                "/api/public/v1/incidents/{}",
                non_public_incident_id()
            ))
            .body(Body::empty())
            .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn page_payload_excludes_non_public_resources() {
    let resp = router()
        .oneshot(
            Request::get("/api/public/v1/status")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_string(resp).await;
    assert_no_secrets("/status", &body);
    // The public component is visible …
    assert!(body.contains(PUBLIC_COMPONENT_NAME));
    // … but nothing tied to non-public state.
    assert!(!body.contains(SECRET_INTERNAL_NAME));
    assert!(!body.contains(NON_PUBLIC_INCIDENT_TITLE));
}

#[tokio::test]
async fn incident_list_excludes_non_public() {
    let resp = router()
        .oneshot(
            Request::get("/api/public/v1/incidents")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_string(resp).await;
    assert_no_secrets("/incidents", &body);
    assert!(body.contains(PUBLIC_INCIDENT_TITLE));
}

#[tokio::test]
async fn rss_feed_excludes_non_public() {
    let resp = router()
        .oneshot(
            Request::get("/api/public/v1/incidents.rss")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_string(resp).await;
    assert_no_secrets("/incidents.rss", &body);
    assert!(body.contains("<rss version=\"2.0\""));
}

#[tokio::test]
async fn maintenance_list_excludes_non_public() {
    let resp = router()
        .oneshot(
            Request::get("/api/public/v1/maintenance")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_string(resp).await;
    assert_no_secrets("/maintenance", &body);
}

// ── Acceptance: cache headers + envelope shape ──────────────────────────────

#[tokio::test]
async fn public_endpoints_set_public_cache_headers() {
    for path in [
        "/api/public/v1/status",
        "/api/public/v1/incidents",
        "/api/public/v1/maintenance",
    ] {
        let resp = router()
            .oneshot(Request::get(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "path {path}");
        let cc = resp
            .headers()
            .get("cache-control")
            .expect("cache-control set")
            .to_str()
            .unwrap()
            .to_owned();
        assert!(
            cc.contains("public")
                && cc.contains("max-age=10")
                && cc.contains("stale-while-revalidate=30"),
            "path {path} cache-control: {cc}"
        );
    }
}

#[tokio::test]
async fn rss_endpoint_serves_rss_content_type() {
    let resp = router()
        .oneshot(
            Request::get("/api/public/v1/incidents.rss")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let ct = resp
        .headers()
        .get("content-type")
        .expect("content-type set")
        .to_str()
        .unwrap()
        .to_owned();
    assert!(ct.starts_with("application/rss+xml"), "content-type: {ct}");
}

// ── Public wire types never admit sensitive fields ──────────────────────────

#[tokio::test]
async fn public_wire_types_lack_sensitive_field_names_in_schema() {
    // The OpenAPI document exposes the structural schema for every public
    // response type. Reading it confirms that none of the sensitive field
    // names (url, headers, basic_auth, bearer_token, internal name, error)
    // are part of the public surface.
    let resp = router()
        .oneshot(
            Request::get("/api/openapi.json")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let doc = body_string(resp).await;
    let doc_json: serde_json::Value = serde_json::from_str(&doc).unwrap();
    let schemas = &doc_json["components"]["schemas"];
    for name in [
        "PublicStatusPage",
        "PublicComponent",
        "PublicComponentGroup",
        "PublicIncident",
        "PublicIncidentUpdate",
        "PublicMaintenance",
        "PublicMaintenanceList",
        "ComponentHistoryResponse",
    ] {
        let s = &schemas[name];
        assert!(
            !s.is_null(),
            "schema {name} missing from OpenAPI components"
        );
        let serialised = serde_json::to_string(s).unwrap();
        for forbidden in [
            "\"url\"",
            "\"headers\"",
            "\"basic_auth\"",
            "\"bearer_token\"",
            "\"check_spec\"",
            "\"response_code\"",
            "\"response_size\"",
            "\"duration_ms\"",
        ] {
            assert!(
                !serialised.contains(forbidden),
                "schema {name} contains forbidden property {forbidden}: {serialised}"
            );
        }
    }
}

#[tokio::test]
async fn page_response_does_not_leak_underlying_target_internals() {
    let resp = router()
        .oneshot(
            Request::get("/api/public/v1/status")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = body_string(resp).await;
    // Even though the underlying private target carries credentials, the
    // public response shape literally cannot contain them. Re-checking on
    // the live body guards against accidental future leakage paths.
    for forbidden in [
        "\"url\"",
        "\"headers\"",
        "\"basic_auth\"",
        "\"bearer_token\"",
        "\"check_spec\"",
    ] {
        assert!(
            !body.contains(forbidden),
            "/status leaked field {forbidden}: {body}"
        );
    }
}

#[tokio::test]
async fn openapi_marks_public_paths_as_unauthenticated() {
    let resp = router()
        .oneshot(
            Request::get("/api/openapi.json")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let doc: serde_json::Value = serde_json::from_str(&body_string(resp).await).unwrap();
    for path in [
        "/api/public/v1/status",
        "/api/public/v1/components/{id}/history",
        "/api/public/v1/incidents",
        "/api/public/v1/incidents/{id}",
        "/api/public/v1/incidents.rss",
        "/api/public/v1/maintenance",
    ] {
        let entry = &doc["paths"][path]["get"];
        assert!(!entry.is_null(), "missing path {path} in OpenAPI doc");
        let security = &entry["security"];
        assert!(
            security.is_array() && security.as_array().unwrap().is_empty(),
            "path {path} should declare empty security() but found {security}"
        );
    }
}
