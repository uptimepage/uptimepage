use std::sync::Arc;

use dashmap::DashMap;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::domain::{CheckSpec, OrgId};

/// Two bulkheads with different contention rules.
///
/// Per-(org, host, port) in-flight cap: tenant-scoped — one customer's burst
/// can't starve another customer's monitor of the same host. Fail-fast:
/// contention returns `Throttled` immediately with no wait, so a pool worker
/// permit is never held while queued. Right for checks that run again in
/// under a minute.
pub struct HostThrottle {
    caps: DashMap<HostKey, Arc<Semaphore>>,
    /// RDAP slots keyed by TLD. Process-wide (not per-tenant) — never burst
    /// the registry. Per-TLD instead of single global so a slow `.com` query
    /// doesn't drag `.org` checks down with it. Queues instead of failing
    /// fast: a daily check dropped at boot paints a region red for a day.
    /// The trade is a pool permit held for at most the check timeout per
    /// queued lookup, cheap against a 1024-permit pool at daily cadence.
    rdap: DashMap<Arc<str>, Arc<Semaphore>>,
    per_host_max: usize,
    rdap_max: usize,
}

pub type HostKey = (OrgId, Arc<str>, u16);

#[must_use = "drop HostPermit only after the check completes — early drop cancels the throttle"]
pub struct HostPermit {
    _inner: OwnedSemaphorePermit,
}

#[derive(Debug)]
pub struct Throttled;

impl HostThrottle {
    pub fn new(per_host_max: usize, rdap_max: usize) -> Self {
        Self {
            caps: DashMap::new(),
            rdap: DashMap::new(),
            per_host_max: clamp_permits(per_host_max),
            rdap_max: clamp_permits(rdap_max),
        }
    }

    /// `None` for DNS (shared resolver pool) and DomainExpiry (uses
    /// `acquire_rdap` instead). Called once at registry refresh — the
    /// returned `Arc<str>` is cached on `ScheduledTarget` and the dispatch
    /// hot path only ever clones the Arc.
    pub fn key_for(org: OrgId, spec: &CheckSpec) -> Option<HostKey> {
        let (host, port) = host_port_raw(spec)?;
        let port = port?;
        Some((org, Arc::from(canonical_host(host)), port))
    }

    /// Lowercase TLD (last label) of a domain. `None` when the input has no
    /// label. Operates on the canonical (IDN-encoded) form so that `bähn.укр`
    /// and `bähn.xn--j1amh` and `BÄHN.укр` collapse onto the same TLD bucket.
    pub fn rdap_tld(domain: &str) -> Option<String> {
        let canonical = canonical_host(domain);
        canonical
            .rsplit('.')
            .next()
            .filter(|t| !t.is_empty())
            .map(str::to_owned)
    }

    /// Wide-open cap for tests + benches.
    pub fn permissive() -> Arc<Self> {
        Arc::new(Self::new(Semaphore::MAX_PERMITS, Semaphore::MAX_PERMITS))
    }

    pub fn acquire(&self, key: &HostKey) -> Result<HostPermit, Throttled> {
        try_acquire(self.semaphore_for(key))
    }

    /// Per-TLD RDAP cap. Caller passes the pre-computed TLD (cached on
    /// `ScheduledTarget.rdap_tld`). Waits FIFO for a free slot; cancel-safe,
    /// so a caller dropped by its check timeout leaves the queue cleanly.
    pub async fn acquire_rdap(&self, tld: &Arc<str>) -> HostPermit {
        let permit = self
            .rdap_semaphore_for(tld)
            .acquire_owned()
            .await
            .expect("rdap semaphore is never closed");
        HostPermit { _inner: permit }
    }

    fn semaphore_for(&self, key: &HostKey) -> Arc<Semaphore> {
        if let Some(s) = self.caps.get(key) {
            return s.clone();
        }
        self.caps
            .entry(key.clone())
            .or_insert_with(|| Arc::new(Semaphore::new(self.per_host_max)))
            .clone()
    }

    fn rdap_semaphore_for(&self, tld: &Arc<str>) -> Arc<Semaphore> {
        if let Some(s) = self.rdap.get(tld) {
            return s.clone();
        }
        self.rdap
            .entry(tld.clone())
            .or_insert_with(|| Arc::new(Semaphore::new(self.rdap_max)))
            .clone()
    }

    /// Off-hot-path eviction across both maps (per-(org, host, port) caps
    /// and per-TLD RDAP slots). Atomic per shard via the shared
    /// `sweep_idle` helper — never drops a semaphore another task just
    /// cloned out.
    pub fn sweep(&self) -> usize {
        let per_host_max = self.per_host_max;
        let rdap_max = self.rdap_max;
        crate::worker::sweep_idle(&self.caps, |sem| sem.available_permits() == per_host_max)
            + crate::worker::sweep_idle(&self.rdap, |sem| sem.available_permits() == rdap_max)
    }

    #[cfg(test)]
    pub fn map_len(&self) -> usize {
        self.caps.len()
    }

    #[cfg(test)]
    pub fn rdap_map_len(&self) -> usize {
        self.rdap.len()
    }
}

fn try_acquire(sem: Arc<Semaphore>) -> Result<HostPermit, Throttled> {
    sem.try_acquire_owned()
        .map(|p| HostPermit { _inner: p })
        .map_err(|_| Throttled)
}

fn clamp_permits(n: usize) -> usize {
    n.clamp(1, Semaphore::MAX_PERMITS)
}

/// Canonical host key: IDN-encoded + ASCII-lowercased + trailing dot stripped.
/// Infallible — falls back to ASCII-lowercase when IDN encoding fails, so the
/// worker hot path never panics on unexpected input.
///
/// Used by:
/// - circuit-breaker key (`host_for_spec`) — `Example.COM`, `example.com.`,
///   `BÄHN.de`, and `xn--bhn-qla.de` share one breaker.
/// - per-(org, host, port) throttle key.
/// - per-TLD RDAP throttle (`rdap_tld`).
/// - cross-tenant RDAP singleflight cache key.
///
/// Use [`canonical_host_strict`] at the API ingest boundary to reject malformed
/// IDN with a 400 instead of silently falling back.
pub fn canonical_host(host: &str) -> String {
    let trimmed = host.trim_end_matches('.');
    idna::domain_to_ascii(trimmed).unwrap_or_else(|_| trimmed.to_ascii_lowercase())
}

/// Strict variant of [`canonical_host`] for use at the API ingest boundary.
/// Uses UTS46 with UseSTD3ASCIIRules, so leading/trailing hyphens, embedded
/// underscores, and other malformed IDN are rejected with a 400 instead of
/// silently stored.
pub fn canonical_host_strict(host: &str) -> Result<String, idna::Errors> {
    idna::domain_to_ascii_strict(host.trim_end_matches('.'))
}

/// Network endpoint of a CheckSpec for variants that target one host+port.
/// HTTP returns its host with `Some(port)` only when `port_or_known_default`
/// can infer one — the breaker is happy with host alone, but the throttle
/// requires both. Ping keys on pseudo-port 0, which validation rejects for
/// every port-bearing kind. `None` for DNS (resolver is the resource) and
/// DomainExpiry (per-TLD RDAP slot is the resource).
pub fn host_port_raw(spec: &CheckSpec) -> Option<(&str, Option<u16>)> {
    match spec {
        CheckSpec::Http(http) => Some((http.url.host_str()?, http.url.port_or_known_default())),
        CheckSpec::Tcp(tcp) => Some((tcp.host.as_str(), Some(tcp.port))),
        CheckSpec::Ping(p) => Some((p.host.as_str(), Some(0))),
        CheckSpec::TlsCert(cert) => Some((cert.host.as_str(), Some(cert.port))),
        CheckSpec::Flow(flow) => Some((
            flow.start_url.host_str()?,
            flow.start_url.port_or_known_default(),
        )),
        CheckSpec::Heartbeat(_) | CheckSpec::Dns(_) | CheckSpec::DomainExpiry(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::time::timeout;
    use uuid::Uuid;

    use super::*;

    fn org() -> OrgId {
        OrgId(Uuid::new_v4())
    }

    fn key(org: OrgId, host: &str) -> HostKey {
        (org, Arc::from(host), 443)
    }

    fn tld(s: &str) -> Arc<str> {
        Arc::from(s)
    }

    #[test]
    fn cap_two_allows_two_concurrent_then_rejects() {
        let throttle = HostThrottle::new(2, 1);
        let org = org();
        let k = key(org, "example.com");
        let _a = throttle.acquire(&k).expect("first");
        let _b = throttle.acquire(&k).expect("second");
        let third = throttle.acquire(&k);
        assert!(third.is_err(), "third acquire above cap must fail-fast");
    }

    #[test]
    fn permit_release_re_opens_slot() {
        let throttle = HostThrottle::new(1, 1);
        let org = org();
        let k = key(org, "example.com");
        {
            let _hold = throttle.acquire(&k).expect("hold");
            assert!(throttle.acquire(&k).is_err());
        }
        assert!(
            throttle.acquire(&k).is_ok(),
            "permit drop must replenish the slot"
        );
    }

    #[test]
    fn tenant_isolation_a_does_not_starve_b() {
        let throttle = HostThrottle::new(1, 1);
        let a = org();
        let b = org();
        let _hold_a = throttle.acquire(&key(a, "example.com")).expect("a");
        let b_permit = throttle.acquire(&key(b, "example.com"));
        assert!(b_permit.is_ok(), "tenant B must not be starved by tenant A");
    }

    #[tokio::test]
    async fn rdap_cap_one_per_tld_queues_and_stays_independent() {
        let throttle = Arc::new(HostThrottle::new(2, 1));
        let hold = throttle.acquire_rdap(&tld("com")).await;
        let waiter = tokio::spawn({
            let throttle = throttle.clone();
            async move { throttle.acquire_rdap(&tld("com")).await }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            !waiter.is_finished(),
            ".com cap=1 must queue the second concurrent acquire"
        );
        assert!(
            timeout(
                Duration::from_millis(50),
                throttle.acquire_rdap(&tld("org"))
            )
            .await
            .is_ok(),
            ".org must not be blocked by a busy .com"
        );
        drop(hold);
        assert!(
            timeout(Duration::from_secs(1), waiter).await.is_ok(),
            "the queued .com waiter must resume once the slot frees"
        );
    }

    #[test]
    fn sweep_drops_unused_entries() {
        let throttle = HostThrottle::new(2, 1);
        let org = org();
        {
            let _p = throttle.acquire(&key(org, "h1")).unwrap();
            let _q = throttle.acquire(&key(org, "h2")).unwrap();
        }
        assert_eq!(throttle.map_len(), 2);
        assert_eq!(throttle.sweep(), 2);
        assert_eq!(throttle.map_len(), 0);
    }

    #[test]
    fn host_normalization_collapses_trailing_dot_and_case() {
        assert_eq!(canonical_host("Example.COM."), "example.com");
        assert_eq!(canonical_host("example.com"), "example.com");
    }

    #[test]
    fn host_normalization_idn_round_trips_to_punycode() {
        let punycode = "xn--bhn-qla.de";
        assert_eq!(canonical_host("Bähn.de"), punycode);
        assert_eq!(canonical_host("BÄHN.de"), punycode);
        assert_eq!(canonical_host("bähn.de."), punycode);
        assert_eq!(canonical_host("xn--bhn-qla.de"), punycode);
    }

    #[test]
    fn canonical_host_strict_rejects_bad_idn() {
        assert!(canonical_host_strict("--invalid-leading.com").is_err());
    }

    #[test]
    fn rdap_tld_extracts_lowercase_last_label() {
        assert_eq!(
            HostThrottle::rdap_tld("example.com").as_deref(),
            Some("com")
        );
        assert_eq!(
            HostThrottle::rdap_tld("EXAMPLE.COM.").as_deref(),
            Some("com")
        );
        assert_eq!(HostThrottle::rdap_tld("uk").as_deref(), Some("uk"));
        assert_eq!(HostThrottle::rdap_tld(""), None);
    }

    #[test]
    fn rdap_tld_collapses_unicode_and_punycode_tld() {
        // Cyrillic .укр and its punycode xn--j1amh map to the same bucket.
        let unicode_tld = HostThrottle::rdap_tld("приклад.укр");
        let puny_tld = HostThrottle::rdap_tld("xn--80aikifvh.xn--j1amh");
        assert_eq!(unicode_tld, puny_tld);
        assert_eq!(unicode_tld.as_deref(), Some("xn--j1amh"));
    }

    #[test]
    fn permissive_does_not_panic_on_construction() {
        let _ = HostThrottle::permissive();
    }
}
