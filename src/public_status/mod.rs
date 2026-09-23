//! Public-status page assembly + cache.
//!
//! Pure status-mapping rules + a single-flight cache + a live aggregator that
//! builds the page payload from PostgreSQL and ClickHouse.

pub mod aggregator;
pub mod badge;
pub mod cache;
pub mod custom_domain_ask;
pub mod incident_writer;
pub mod logo_storage;
pub mod overall_status;
pub mod source;
pub mod subscriber_dispatch;
pub mod urls;
pub mod xml;

pub use aggregator::{AggregatorConfig, OrgAggregator};
pub use cache::{HistoryIncidentMarker, PageCache, PageCacheError};

pub use incident_writer::{
    InMemoryIncidentStore, IncidentStore, IncidentWriter, IncidentWriterConfig, NewOpenIncident,
    OpenIncident, PgIncidentStore,
};
pub use logo_storage::LogoMime;
pub use source::{IncidentListQuery, NoopPublicSource, OrgPublicSource, PublicSource};

/// `"{name} Status"`, unless the name already ends in the word "status".
pub fn status_title(name: &str) -> String {
    let name = name.trim_end();
    if names_status(name) {
        name.to_owned()
    } else {
        format!("{name} Status")
    }
}

fn names_status(name: &str) -> bool {
    name.trim_end_matches(|c: char| !c.is_alphanumeric())
        .rsplit(|c: char| !c.is_alphanumeric())
        .next()
        .is_some_and(|w| w.eq_ignore_ascii_case("status"))
}

#[cfg(test)]
mod tests {
    use super::status_title;

    #[test]
    fn status_title_does_not_repeat_a_trailing_status() {
        assert_eq!(status_title("Acme"), "Acme Status");
        assert_eq!(status_title("Acme status"), "Acme status");
        assert_eq!(status_title("ACME STATUS "), "ACME STATUS");
        assert_eq!(status_title("Acme · Status"), "Acme · Status");
        assert_eq!(status_title("Acme Status."), "Acme Status.");
        assert_eq!(status_title("Acme (Status)"), "Acme (Status)");
        assert_eq!(status_title("Statuspage"), "Statuspage Status");
    }
}
