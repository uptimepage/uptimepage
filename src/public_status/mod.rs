//! Public-status page assembly + cache.
//!
//! Pure status-mapping rules + a single-flight cache + a live aggregator that
//! builds the page payload from PostgreSQL and ClickHouse.

pub mod aggregator;
pub mod badge;
pub mod cache;
pub mod incident_writer;
pub mod logo_storage;
pub mod overall_status;
pub mod source;
pub mod subscriber_dispatch;
pub mod urls;
pub mod xml;

pub use aggregator::{AggregatorConfig, OrgAggregator};
pub use cache::{HistoryIncidentMarker, PageCache, PageCacheError};

/// Auto-generated incident title used when the operator did not set
/// `public_title`. Single source so all three load sites stay aligned —
/// previously `source.rs::hydrate` was missing the underscore→space step.
pub fn auto_incident_title(component_name: &str, status_at_start: &str) -> String {
    format!("{} {}", component_name, status_at_start.replace('_', " "))
}
pub use incident_writer::{
    InMemoryIncidentStore, IncidentStore, IncidentWriter, IncidentWriterConfig, NewOpenIncident,
    OpenIncident, PgIncidentStore,
};
pub use logo_storage::LogoMime;
pub use source::{IncidentListQuery, NoopPublicSource, OrgPublicSource, PublicSource};
