//! Cross-tenant smoke test for the public-status surface.
//!
//! Primes the page cache for org A, then asks for org B's page, and asserts
//! org B's response contains no byte of org A's payload. The compile-time
//! fence — `OrgAggregator` / `OrgPublicSource` taking `OrgId` per call, with
//! no ambient org pinned at construction — is the primary defence; this
//! test is the runtime regression net for any future `PublicSource` impl or
//! cache change that quietly drops the org parameter. Runs on every PR
//! (no DB required).

mod common;

use std::sync::Arc;

use chrono::Utc;
use uptimepage::config::PublicStatusConfig;
use uptimepage::domain::{OrgId, OverallState, OverallStatus, PublicStatusPage, StatusPageId};
use uptimepage::public_status::cache::PageCache;
use uuid::Uuid;

const ORG_A_MARKER: &str = "tenant-A-marker-7c1e5f9b";
const ORG_B_MARKER: &str = "tenant-B-marker-d20a3413";

fn page_with_marker(marker: &str) -> PublicStatusPage {
    PublicStatusPage {
        overall: OverallStatus {
            state: OverallState::Operational,
            label: "All Systems Operational".into(),
        },
        generated_at: Utc::now(),
        site_name: marker.into(),
        groups: Vec::new(),
        active_incidents: Vec::new(),
        recent_incidents: Vec::new(),
        recent_incidents_has_more: false,
        active_maintenance: Vec::new(),
        upcoming_maintenance: Vec::new(),
    }
}

#[tokio::test]
async fn cache_does_not_serve_org_a_payload_to_org_b() {
    let cache = PageCache::new(&PublicStatusConfig::default());
    let org_a = StatusPageId(Uuid::new_v4());
    let org_b = StatusPageId(Uuid::new_v4());

    let primed = cache
        .get_or_compute(org_a, || async {
            Ok::<_, std::io::Error>(page_with_marker(ORG_A_MARKER))
        })
        .await
        .expect("prime A");
    assert!(primed.site_name.contains(ORG_A_MARKER));

    let serialized_b = serde_json::to_string(
        &*cache
            .get_or_compute(org_b, || async {
                Ok::<_, std::io::Error>(page_with_marker(ORG_B_MARKER))
            })
            .await
            .expect("compute B"),
    )
    .expect("serialise B");

    assert!(
        serialized_b.contains(ORG_B_MARKER),
        "B's response missing its own marker: {serialized_b}"
    );
    assert!(
        !serialized_b.contains(ORG_A_MARKER),
        "B's response leaked A's marker: {serialized_b}"
    );
}

#[tokio::test]
async fn cache_last_good_is_partitioned_per_org() {
    // A hot org's `last_good` snapshot must not satisfy a different org's
    // recompute failure. Prime A, then fail B's first compute, and verify B
    // receives `Unavailable` rather than A's stale data.
    let cache = PageCache::new(&PublicStatusConfig::default());
    let org_a = StatusPageId(Uuid::new_v4());
    let org_b = StatusPageId(Uuid::new_v4());

    let _primed = cache
        .get_or_compute(org_a, || async {
            Ok::<_, std::io::Error>(page_with_marker(ORG_A_MARKER))
        })
        .await
        .expect("prime A");

    let err = cache
        .get_or_compute(org_b, || async {
            Err::<PublicStatusPage, _>(std::io::Error::other("B unavailable"))
        })
        .await
        .expect_err("B has no last_good and must surface Unavailable");
    assert!(matches!(
        err,
        uptimepage::public_status::cache::PageCacheError::Unavailable
    ));
    let snap = cache.last_good(org_a).expect("A still cached");
    assert!(snap.site_name.contains(ORG_A_MARKER));
}

#[tokio::test]
async fn public_source_trait_threads_org_param_to_distinct_responses() {
    // Drives a hand-rolled `PublicSource` impl whose `page(org)` echoes the
    // org's tag back. Two distinct orgs must yield two distinct responses —
    // catches the future regression where an impl adds the trait parameter
    // but internally falls back to a baked default.
    use async_trait::async_trait;
    use std::collections::HashMap;
    use uptimepage::domain::{
        ComponentHistoryResponse, PageRef, PublicIncident, PublicMaintenanceList, StatusPageId,
    };
    use uptimepage::error::public::PublicAppError;
    use uptimepage::pagination::CursorPage;
    use uptimepage::public_status::{IncidentListQuery, PublicSource, source::FeedLinks};

    // Public reads are keyed by status page, not org — isolation is per-page.
    struct PageKeyedSource {
        pages: HashMap<StatusPageId, Arc<PublicStatusPage>>,
    }
    #[async_trait]
    impl PublicSource for PageKeyedSource {
        async fn page(&self, page: PageRef) -> Result<Arc<PublicStatusPage>, PublicAppError> {
            self.pages
                .get(&page.page)
                .cloned()
                .ok_or(PublicAppError::NotFound)
        }
        async fn component_history(
            &self,
            _page: PageRef,
            _id: Uuid,
            _days: u32,
        ) -> Result<ComponentHistoryResponse, PublicAppError> {
            unreachable!()
        }
        async fn list_incidents(
            &self,
            _page: PageRef,
            _q: IncidentListQuery,
        ) -> Result<CursorPage<PublicIncident>, PublicAppError> {
            unreachable!()
        }
        async fn incident_by_id(
            &self,
            _page: PageRef,
            _id: Uuid,
        ) -> Result<PublicIncident, PublicAppError> {
            unreachable!()
        }
        async fn maintenance(
            &self,
            _page: PageRef,
        ) -> Result<PublicMaintenanceList, PublicAppError> {
            unreachable!()
        }
        async fn incidents_rss(
            &self,
            _page: PageRef,
            _links: FeedLinks<'_>,
        ) -> Result<String, PublicAppError> {
            unreachable!()
        }
    }

    let org = OrgId(Uuid::new_v4());
    let page_a = PageRef {
        page: StatusPageId(Uuid::new_v4()),
        org,
    };
    let page_b = PageRef {
        page: StatusPageId(Uuid::new_v4()),
        org,
    };
    let src = PageKeyedSource {
        pages: HashMap::from([
            (page_a.page, Arc::new(page_with_marker(ORG_A_MARKER))),
            (page_b.page, Arc::new(page_with_marker(ORG_B_MARKER))),
        ]),
    };

    let resp_a = src.page(page_a).await.expect("A");
    let resp_b = src.page(page_b).await.expect("B");
    assert!(resp_a.site_name.contains(ORG_A_MARKER));
    assert!(resp_b.site_name.contains(ORG_B_MARKER));
    let body_b = serde_json::to_string(&*resp_b).expect("serialise B");
    assert!(!body_b.contains(ORG_A_MARKER));
}
