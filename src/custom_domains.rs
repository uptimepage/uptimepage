//! In-memory set of the custom domains this deployment serves. The host
//! decisions that read it are sync middleware with no pool; the rebuild lives
//! in `jobs::custom_domains`.

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use arc_swap::ArcSwap;
use chrono::{DateTime, Utc};

use crate::domain::{PageRef, StatusPageId};

const MAX_HOST_LEN: usize = 253;

#[derive(Debug, Clone)]
pub struct CustomDomainRow {
    pub domain: String,
    pub page: PageRef,
    pub slug: String,
    pub activated: bool,
}

#[derive(Default)]
struct Snapshot {
    serving: HashMap<Arc<str>, PageRef>,
    published: HashMap<StatusPageId, Arc<str>>,
    subdomain: HashMap<StatusPageId, Arc<str>>,
}

pub struct CustomDomains {
    base_domain: String,
    snapshot: ArcSwap<Snapshot>,
    /// Unix seconds; 0 until the first install.
    loaded_at: AtomicI64,
    refresh: tokio::sync::Mutex<()>,
}

impl CustomDomains {
    pub fn new(base_domain: &str) -> Self {
        Self {
            base_domain: base_domain.trim().to_ascii_lowercase(),
            snapshot: ArcSwap::from_pointee(Snapshot::default()),
            loaded_at: AtomicI64::new(0),
            refresh: tokio::sync::Mutex::new(()),
        }
    }

    pub fn lookup(&self, host: &str) -> Option<PageRef> {
        let host = normalize(host)?;
        self.snapshot.load().serving.get(host.as_ref()).copied()
    }

    /// `None` until the page is activated, not merely verified.
    pub fn published(&self, page: StatusPageId) -> Option<Arc<str>> {
        self.snapshot.load().published.get(&page).cloned()
    }

    pub fn subdomain(&self, page: StatusPageId) -> Option<Arc<str>> {
        self.snapshot.load().subdomain.get(&page).cloned()
    }

    /// Returns how many rows survived.
    pub fn install(&self, rows: Vec<CustomDomainRow>) -> usize {
        let mut snapshot = Snapshot::default();
        let accepted: Vec<(Arc<str>, CustomDomainRow)> = rows
            .into_iter()
            .filter_map(|row| {
                let Some(host) = normalize(&row.domain).filter(|h| !self.is_our_own(h)) else {
                    tracing::warn!(
                        page = %row.page.page.0,
                        "custom domain refused: not a legal DNS name, or inside this deployment's own base domain"
                    );
                    return None;
                };
                Some((Arc::from(host.as_ref()), row))
            })
            .collect();

        // Serving a contested host would put one tenant's readers on another
        // tenant's page, so neither page gets it.
        let mut claims: HashMap<&str, usize> = HashMap::new();
        for (host, _) in &accepted {
            *claims.entry(host.as_ref()).or_default() += 1;
        }

        for (host, row) in &accepted {
            if claims.get(host.as_ref()).copied().unwrap_or(0) > 1 {
                tracing::error!(
                    page = %row.page.page.0,
                    host = %host,
                    "custom domain refused: more than one page resolves to this host"
                );
                continue;
            }
            let (host, row) = (Arc::clone(host), row);
            if row.activated {
                snapshot.published.insert(row.page.page, Arc::clone(&host));
            }
            if !self.base_domain.is_empty() {
                snapshot.subdomain.insert(
                    row.page.page,
                    Arc::from(format!("{}.{}", row.slug, self.base_domain).as_str()),
                );
            }
            snapshot.serving.insert(host, row.page);
        }
        let n = snapshot.serving.len();
        self.snapshot.store(Arc::new(snapshot));
        self.loaded_at
            .store(Utc::now().timestamp(), Ordering::Relaxed);
        n
    }

    pub fn len(&self) -> usize {
        self.snapshot.load().serving.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// `None` until the first successful load.
    pub fn loaded_at(&self) -> Option<DateTime<Utc>> {
        match self.loaded_at.load(Ordering::Relaxed) {
            0 => None,
            secs => DateTime::from_timestamp(secs, 0),
        }
    }

    /// Apex or anything under it at any depth. `parse_host_shape` is not this
    /// test — it answers `Other` for `a.b.{base}`, a name in a zone we control.
    fn is_our_own(&self, host: &str) -> bool {
        if self.base_domain.is_empty() {
            return false;
        }
        host == self.base_domain
            || host
                .strip_suffix(&self.base_domain)
                .is_some_and(|head| head.ends_with('.'))
    }

    /// Held across the query and the install so two rebuilds cannot land out
    /// of order.
    pub async fn refresh_guard(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.refresh.lock().await
    }
}

const MAX_LABEL_LEN: usize = 63;

/// Lower case, no port, no trailing dot, punycode, legal DNS labels. Run on
/// install and on lookup, so a stored domain cannot miss a request spelling it
/// another way.
pub fn normalize(host: &str) -> Option<Cow<'_, str>> {
    let host = host.split(':').next().unwrap_or(host);
    let host = host.strip_suffix('.').unwrap_or(host);
    if host.len() > MAX_HOST_LEN {
        return None;
    }
    let ascii = if host
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'.')
    {
        Cow::Borrowed(host)
    } else {
        // UTS46 maps case as well as IDN, so this is also the uppercase path.
        Cow::Owned(idna::domain_to_ascii(host).ok()?)
    };

    has_legal_labels(&ascii).then_some(ascii)
}

/// `idna::domain_to_ascii` passes `a..b`, `-lead.test` and over-length labels,
/// and the fast path above never calls it at all. Migration 075 enforces the
/// same shape on the stored column.
fn has_legal_labels(host: &str) -> bool {
    let mut labels = 0;
    for label in host.split('.') {
        labels += 1;
        let legal = (1..=MAX_LABEL_LEN).contains(&label.len())
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
        if !legal {
            return false;
        }
    }
    labels >= 2
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::OrgId;

    fn page_ref(n: u128) -> PageRef {
        PageRef {
            page: StatusPageId(uuid::Uuid::from_u128(n)),
            org: OrgId(uuid::Uuid::from_u128(n + 1000)),
        }
    }

    fn row(domain: &str, n: u128, activated: bool) -> CustomDomainRow {
        CustomDomainRow {
            domain: domain.into(),
            page: page_ref(n),
            slug: format!("page{n}"),
            activated,
        }
    }

    #[test]
    fn normalize_leaves_a_canonical_host_borrowed() {
        assert!(matches!(
            normalize("status.acme.test"),
            Some(Cow::Borrowed("status.acme.test"))
        ));
    }

    #[test]
    fn normalize_strips_port_case_and_the_fqdn_dot() {
        for host in [
            "Status.ACME.test",
            "status.acme.test:443",
            "status.acme.test.",
            "STATUS.acme.test.:8443",
        ] {
            assert_eq!(
                normalize(host).as_deref(),
                Some("status.acme.test"),
                "{host}"
            );
        }
    }

    #[test]
    fn normalize_punycodes_an_idn() {
        assert_eq!(
            normalize("stätus.acme.test").as_deref(),
            Some("xn--sttus-hra.acme.test")
        );
        assert_eq!(
            normalize("xn--sttus-hra.acme.test").as_deref(),
            Some("xn--sttus-hra.acme.test")
        );
    }

    #[test]
    fn normalize_refuses_what_is_not_a_domain() {
        for host in ["", "localhost", "status", ".", ":443", "[::1]:443"] {
            assert_eq!(normalize(host), None, "{host}");
        }
        assert_eq!(normalize(&format!("{}.test", "a".repeat(250))), None);
    }

    #[test]
    fn normalize_refuses_a_malformed_dns_name() {
        for host in [
            "status..customer.test",
            "-lead.customer.test",
            "trail-.customer.test",
            "status.customer.test-",
            ".customer.test",
            "status_page.customer.test",
            "STATUS_.customer.test",
        ] {
            assert_eq!(normalize(host), None, "{host}");
        }
        let long = format!("{}.customer.test", "a".repeat(64));
        assert_eq!(normalize(&long), None);
        let ok = format!("{}.customer.test", "a".repeat(63));
        assert!(normalize(&ok).is_some());
    }

    #[test]
    fn two_pages_on_one_host_leave_neither_serving() {
        let d = CustomDomains::new("example.com");
        assert_eq!(
            d.install(vec![
                row("status.acme.test", 1, true),
                row("status.acme.test.", 2, true),
            ]),
            0
        );
        assert_eq!(d.lookup("status.acme.test"), None);
        assert_eq!(d.published(page_ref(1).page), None);
        assert_eq!(d.published(page_ref(2).page), None);
    }

    #[test]
    fn one_ambiguous_host_does_not_take_the_others_down() {
        let d = CustomDomains::new("example.com");
        assert_eq!(
            d.install(vec![
                row("status.acme.test", 1, true),
                row("status.acme.test.", 2, true),
                row("status.other.test", 3, true),
            ]),
            1
        );
        assert_eq!(d.lookup("status.other.test"), Some(page_ref(3)));
    }

    #[test]
    fn lookup_matches_however_the_request_spells_it() {
        let d = CustomDomains::new("example.com");
        assert_eq!(d.install(vec![row("Status.ACME.test.", 1, false)]), 1);
        for host in [
            "status.acme.test",
            "STATUS.acme.test",
            "status.acme.test:443",
            "status.acme.test.",
        ] {
            assert_eq!(d.lookup(host), Some(page_ref(1)), "{host}");
        }
        assert_eq!(d.lookup("other.acme.test"), None);
    }

    #[test]
    fn install_drops_a_row_anywhere_inside_our_own_base_domain() {
        let d = CustomDomains::new("example.com");
        assert_eq!(
            d.install(vec![
                row("app.example.com", 1, true),
                row("acme.example.com", 2, true),
                row("example.com", 3, true),
                row("a.b.example.com", 4, true),
                row("status.acme.test", 5, true),
            ]),
            1
        );
        for host in [
            "app.example.com",
            "acme.example.com",
            "example.com",
            "a.b.example.com",
        ] {
            assert_eq!(d.lookup(host), None, "{host}");
        }
        assert_eq!(d.lookup("status.acme.test"), Some(page_ref(5)));
    }

    #[test]
    fn a_lookalike_outside_our_zone_is_still_a_customers_to_claim() {
        let d = CustomDomains::new("example.com");
        assert_eq!(d.install(vec![row("status.notexample.com", 1, false)]), 1);
        assert_eq!(d.lookup("status.notexample.com"), Some(page_ref(1)));
    }

    #[test]
    fn a_served_page_carries_its_own_subdomain() {
        let d = CustomDomains::new("example.com");
        d.install(vec![row("status.acme.test", 1, false)]);
        assert_eq!(
            d.subdomain(page_ref(1).page).as_deref(),
            Some("page1.example.com")
        );
    }

    #[test]
    fn a_verified_domain_serves_before_it_publishes() {
        let d = CustomDomains::new("example.com");
        d.install(vec![row("status.acme.test", 1, false)]);
        assert_eq!(d.lookup("status.acme.test"), Some(page_ref(1)));
        assert_eq!(d.published(page_ref(1).page), None);

        d.install(vec![row("status.acme.test", 1, true)]);
        assert_eq!(
            d.published(page_ref(1).page).as_deref(),
            Some("status.acme.test")
        );
    }

    #[test]
    fn install_replaces_rather_than_merges() {
        let d = CustomDomains::new("example.com");
        d.install(vec![row("status.acme.test", 1, true)]);
        d.install(vec![row("status.other.test", 2, true)]);
        assert_eq!(d.lookup("status.acme.test"), None);
        assert_eq!(d.published(page_ref(1).page), None);
        assert_eq!(d.lookup("status.other.test"), Some(page_ref(2)));
    }

    #[test]
    fn loaded_at_is_unset_until_the_first_install() {
        let d = CustomDomains::new("example.com");
        assert!(d.loaded_at().is_none());
        assert!(d.is_empty());
        d.install(Vec::new());
        assert!(d.loaded_at().is_some(), "an empty result is still a load");
    }
}
