//! Domain-expiry probe with sticky last-good fallback.
//!
//! Each scheduled check follows the same path:
//!  1. Fresh-probe attempt: per-TLD bulkhead + cross-tenant singleflight.
//!  2. On success: write the answer to `domain_expiry_state` (last-good
//!     cache) and emit a CheckResult whose status is derived from
//!     `classify_days`.
//!  3. On failure (timeout, network, registry error): load last-good. If
//!     younger than `max_staleness`, emit a CheckResult with the *cached*
//!     status and an `error="served_stale: …"` annotation so operator tools
//!     can see we served stale data.
//!  4. If the row is missing or older than `max_staleness`, emit a real
//!     `CheckStatus::Error` with the underlying failure message.
//!
//! Industry shape mirrors Better Stack / Site24x7 domain monitors: a
//! transient registry blip never flips the customer's monitor red, and a
//! genuinely-unreachable registry surfaces only after the staleness ceiling.

use std::sync::Arc;
use std::sync::LazyLock;
use std::time::Duration;

use anyhow::anyhow;
use chrono::Utc;
use metrics::{Counter, counter};
use serde::Serialize;
use tokio::time::{Instant, timeout_at};
use uuid::Uuid;

static HOST_THROTTLE_WAITS_RDAP: LazyLock<Counter> =
    LazyLock::new(|| counter!(metric_names::HOST_THROTTLE_WAITS, "kind" => "rdap"));
static RDAP_SINGLEFLIGHT_HITS: LazyLock<Counter> =
    LazyLock::new(|| counter!(metric_names::RDAP_SINGLEFLIGHT, "outcome" => "hit"));
static RDAP_SINGLEFLIGHT_MISSES: LazyLock<Counter> =
    LazyLock::new(|| counter!(metric_names::RDAP_SINGLEFLIGHT, "outcome" => "miss"));
static STATE_WRITE_FAILED: LazyLock<Counter> =
    LazyLock::new(|| counter!(metric_names::DOMAIN_EXPIRY_STATE_WRITE_FAILED));

use crate::domain::{CheckResult, CheckStatus, DomainExpiryCheck, OrgId, SERVED_STALE_PREFIX};
use crate::http_client::HttpClients;
use crate::metric_names;
use crate::storage::DomainExpiryStateStore;
use crate::worker::host_throttle::HostThrottle;
use crate::worker::rdap_singleflight::{FetchOutcome, RdapSingleflight};
use crate::worker::registration::{
    RegistrationAnswer, RegistrationClient, RegistrationError, tld_verdict,
};

/// Default ceiling on how old a cached last-good answer may be while still
/// being served. Past this, the executor escalates to `CheckStatus::Error`
/// so an alert can fire on truly-unreachable registries.
pub const DEFAULT_MAX_STALENESS: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// Bundle of dependencies the domain-expiry executor needs at dispatch
/// time. Built once and shared across every probe via `Arc`.
pub struct DomainExpiryRuntime {
    pub registry_client: Arc<RegistrationClient>,
    pub singleflight: Arc<RdapSingleflight>,
    pub state_store: Arc<dyn DomainExpiryStateStore>,
    pub host_throttle: Arc<HostThrottle>,
    max_staleness_chrono: chrono::Duration,
}

impl DomainExpiryRuntime {
    pub fn new(
        registry_client: Arc<RegistrationClient>,
        singleflight: Arc<RdapSingleflight>,
        state_store: Arc<dyn DomainExpiryStateStore>,
        host_throttle: Arc<HostThrottle>,
        max_staleness: Duration,
    ) -> Self {
        let max_staleness_chrono = chrono::Duration::from_std(max_staleness)
            .expect("max_staleness fits chrono::Duration (caller passes a sane bound)");
        Self {
            registry_client,
            singleflight,
            state_store,
            host_throttle,
            max_staleness_chrono,
        }
    }

    pub fn max_staleness(&self) -> chrono::Duration {
        self.max_staleness_chrono
    }
}

pub async fn execute_domain_expiry_check(
    target_id: Uuid,
    org_id: Uuid,
    check: &DomainExpiryCheck,
    runtime: &DomainExpiryRuntime,
    clients: &HttpClients,
) -> CheckResult {
    let started_at = Utc::now();
    let start = Instant::now();

    let probe = fresh_probe(check, runtime, clients).await;
    let duration_ms = start.elapsed().as_millis() as u32;

    match probe {
        Ok(answer) => {
            if let Err(e) = runtime
                .state_store
                .upsert_success(
                    OrgId(org_id),
                    target_id,
                    &check.domain,
                    answer.expiration,
                    answer.registrar.as_deref(),
                )
                .await
            {
                // Silent swallow would freeze last_success_at while probes
                // keep succeeding, eventually escalating future failures to
                // Error instead of serving the (still-fresh) cached answer.
                tracing::warn!(%target_id, error = %e, "domain_expiry: upsert_success failed");
                STATE_WRITE_FAILED.increment(1);
            }
            let verdict = classify(check, answer.expiration, answer.registrar.as_deref());
            emit_fresh(target_id, org_id, started_at, duration_ms, verdict)
        }
        Err(err) => {
            fall_back(
                target_id,
                org_id,
                check,
                runtime,
                started_at,
                duration_ms,
                err,
            )
            .await
        }
    }
}

/// Outcome of the fresh-probe attempt — distinguished from a generic
/// `Result` so the fallback path can record the right metric kind.
#[derive(Debug)]
enum ProbeFailure {
    Timeout,
    Lookup(anyhow::Error),
    /// Retrying cannot change it, so the message names the cause.
    Permanent(RegistrationError),
}

/// A queued lookup whose per-TLD slot came free only after the check
/// deadline. Starting the outbound request then would be exactly the
/// registry burst the cap exists to prevent, so the fetcher bails first.
#[derive(Debug, thiserror::Error)]
#[error("rdap timeout")]
struct QueueExpired;

impl ProbeFailure {
    fn kind(&self) -> &'static str {
        match self {
            Self::Timeout => "timeout",
            Self::Lookup(_) => "lookup_error",
            Self::Permanent(_) => "unsupported_tld",
        }
    }
    fn message(&self) -> String {
        match self {
            Self::Timeout => "rdap timeout".into(),
            Self::Lookup(e) => e.to_string(),
            Self::Permanent(e) => e.to_string(),
        }
    }
}

async fn fresh_probe(
    check: &DomainExpiryCheck,
    runtime: &DomainExpiryRuntime,
    clients: &HttpClients,
) -> std::result::Result<Arc<RegistrationAnswer>, ProbeFailure> {
    // Canonicalise both the singleflight key AND the upstream lookup target
    // — `Bähn.de`, `BÄHN.de`, and `xn--bhn-qla.de` must share one slot, one
    // outbound RDAP call, and one per-TLD permit. Without this, the
    // ingest-side canonicalisation could be defeated by anything that fed a
    // raw user string here (e.g. a future test/admin path).
    let canonical = crate::worker::host_throttle::canonical_host(&check.domain);
    let domain: Arc<str> = Arc::from(canonical.as_str());
    let tld = HostThrottle::rdap_tld(&canonical).map(Arc::<str>::from);
    let deadline = Instant::now() + check.timeout;

    // Throttle gate sits INSIDE the fetcher closure so a cache hit never
    // consumes a per-TLD permit and never bumps the wait counter — the
    // bulkhead exists to protect registries from outbound traffic, and a
    // hit makes no outbound traffic. One deadline covers the queue wait and
    // the lookup, and a waiter that reaches the slot with nothing left never
    // opens a connection it would abort a moment later.
    let client = runtime.registry_client.clone();
    let lookup_domain = domain.clone();
    let host_throttle = runtime.host_throttle.clone();
    let lookup = runtime.singleflight.lookup(domain, move || async move {
        let _permit = match tld.as_ref() {
            Some(t) => {
                HOST_THROTTLE_WAITS_RDAP.increment(1);
                let permit = host_throttle.acquire_rdap(t).await;
                if Instant::now() >= deadline {
                    return Err(crate::error::AppError::Other(QueueExpired.into()));
                }
                Some(permit)
            }
            None => None,
        };
        client
            .lookup_expiration(lookup_domain.as_ref(), clients)
            .await
    });

    let outcome = timeout_at(deadline, lookup).await;

    match outcome {
        Ok(Ok((answer, fetch_outcome))) => {
            record_singleflight_outcome(fetch_outcome);
            Ok(answer)
        }
        Ok(Err(crate::error::AppError::Other(e))) => {
            if let Some(verdict) = tld_verdict(&e) {
                Err(ProbeFailure::Permanent(verdict.clone()))
            } else if e.downcast_ref::<QueueExpired>().is_some() {
                Err(ProbeFailure::Timeout)
            } else {
                Err(ProbeFailure::Lookup(e))
            }
        }
        Ok(Err(e)) => Err(ProbeFailure::Lookup(anyhow!(e.to_string()))),
        Err(_) => Err(ProbeFailure::Timeout),
    }
}

async fn fall_back(
    target_id: Uuid,
    org_id: Uuid,
    check: &DomainExpiryCheck,
    runtime: &DomainExpiryRuntime,
    started_at: chrono::DateTime<Utc>,
    duration_ms: u32,
    err: ProbeFailure,
) -> CheckResult {
    let err_kind = err.kind();
    let err_msg = err.message();

    // Single round-trip: UPDATE … RETURNING bumps the failure counters AND
    // hands back the current row. Decides staleness in app-space.
    let state = match runtime
        .state_store
        .record_failure_returning(OrgId(org_id), target_id, &err_msg)
        .await
    {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(%target_id, error = %e, "domain_expiry: state store write failed");
            None
        }
    };

    if let Some(state) = state {
        let age = Utc::now() - state.last_success_at;
        let age_secs = age.num_seconds().max(0) as u64;
        if age <= runtime.max_staleness_chrono {
            tracing::debug!(
                %target_id,
                age_secs,
                kind = err_kind,
                "domain_expiry: serving stale last-good"
            );
            counter!(metric_names::DOMAIN_EXPIRY_STALE_SERVED, "kind" => err_kind).increment(1);
            let verdict = classify(check, state.expiry_at, state.registrar.as_deref());
            return emit_stale(
                target_id,
                org_id,
                started_at,
                duration_ms,
                verdict,
                age_secs,
                err_kind,
            );
        }
    }

    counter!(metric_names::DOMAIN_EXPIRY_STALE_SERVED, "kind" => "fresh_error").increment(1);
    tracing::debug!(
        %target_id,
        kind = err_kind,
        "domain_expiry: no usable last-good (missing or beyond staleness ceiling), emitting fresh Error"
    );
    CheckResult::error_with_elapsed(target_id, org_id, started_at, duration_ms, err_msg)
}

fn record_singleflight_outcome(outcome: FetchOutcome) {
    match outcome {
        FetchOutcome::Hit => RDAP_SINGLEFLIGHT_HITS.increment(1),
        FetchOutcome::Miss => RDAP_SINGLEFLIGHT_MISSES.increment(1),
    }
}

#[derive(Debug)]
struct Verdict {
    status: CheckStatus,
    details_json: String,
}

fn classify(
    check: &DomainExpiryCheck,
    expiration: chrono::DateTime<Utc>,
    registrar: Option<&str>,
) -> Verdict {
    let days_remaining = (expiration - Utc::now()).num_days();
    let status = crate::worker::classify_days(days_remaining, check.warn_days, check.critical_days);

    #[derive(Serialize)]
    struct Details<'a> {
        domain: &'a str,
        days_remaining: i64,
        expiration_date: String,
        registrar: Option<&'a str>,
    }
    let details_json = serde_json::to_string(&Details {
        domain: &check.domain,
        days_remaining,
        expiration_date: expiration.to_rfc3339(),
        registrar,
    })
    .expect("infallible serialize for fixed struct");
    Verdict {
        status,
        details_json,
    }
}

fn emit_fresh(
    target_id: Uuid,
    org_id: Uuid,
    started_at: chrono::DateTime<Utc>,
    duration_ms: u32,
    verdict: Verdict,
) -> CheckResult {
    CheckResult {
        target_id,
        org_id,
        timestamp: started_at,
        status: verdict.status,
        duration_ms,
        dns_ms: None,
        connect_ms: None,
        tls_ms: None,
        ttfb_ms: None,
        response_code: None,
        response_size: Some(verdict.details_json.len() as u32),
        diagnostic: None,
        error: match verdict.status {
            CheckStatus::Up => None,
            _ => Some(verdict.details_json),
        },
    }
}

fn emit_stale(
    target_id: Uuid,
    org_id: Uuid,
    started_at: chrono::DateTime<Utc>,
    duration_ms: u32,
    verdict: Verdict,
    age_secs: u64,
    refresh_failure_kind: &str,
) -> CheckResult {
    // On Up, the customer's domain is fine for the next N days regardless of
    // where the answer came from — leave `error` empty so renderers that
    // surface non-empty `error` as a warning don't mis-classify a healthy
    // cached verdict. Operators still see the stale-served event via the
    // `uptimepage_domain_expiry_stale_served_total` counter.
    let error = match verdict.status {
        CheckStatus::Up => None,
        _ => {
            let annotation = format!(
                "{SERVED_STALE_PREFIX} last_verified_age_secs={age_secs}; refresh_failed={refresh_failure_kind}",
            );
            Some(format!("{annotation}; {}", verdict.details_json))
        }
    };
    CheckResult {
        target_id,
        org_id,
        timestamp: started_at,
        status: verdict.status,
        duration_ms,
        dns_ms: None,
        connect_ms: None,
        tls_ms: None,
        ttfb_ms: None,
        response_code: None,
        response_size: error.as_ref().map(|e| e.len() as u32),
        diagnostic: None,
        error,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::InMemoryDomainExpiryStateStore;
    use crate::worker::host_throttle::HostThrottle;
    use crate::worker::rdap::RdapClient;
    use std::time::Duration as StdDuration;

    fn check(domain: &str) -> DomainExpiryCheck {
        DomainExpiryCheck {
            domain: domain.into(),
            warn_days: 30,
            critical_days: 7,
            timeout: StdDuration::from_secs(5),
        }
    }

    fn seed_state(store: &InMemoryDomainExpiryStateStore, org: OrgId, target: Uuid, days: i64) {
        let exp = Utc::now() + chrono::Duration::days(days);
        futures::executor::block_on(store.upsert_success(
            org,
            target,
            "example.com",
            exp,
            Some("R"),
        ))
        .unwrap();
    }

    #[tokio::test]
    async fn beyond_staleness_emits_real_error() {
        let target = Uuid::new_v4();
        let org = OrgId(Uuid::new_v4());
        let store: Arc<InMemoryDomainExpiryStateStore> =
            Arc::new(InMemoryDomainExpiryStateStore::new());
        seed_state(&store, org, target, 90);
        {
            let mut g = store.inner_mut_for_test();
            let s = g.get_mut(&(org, target)).unwrap();
            s.last_success_at = Utc::now() - chrono::Duration::days(10);
        }

        let runtime = DomainExpiryRuntime::new(
            Arc::new(RegistrationClient::new(Arc::new(RdapClient::new(
                crate::http_outbound::build_outbound_client(crate::security::SsrfGuard::strict()),
            )))),
            Arc::new(RdapSingleflight::with_default_ttl()),
            store,
            HostThrottle::permissive(),
            StdDuration::from_secs(7 * 24 * 3600),
        );
        let r = fall_back(
            target,
            org.0,
            &check("example.com"),
            &runtime,
            Utc::now(),
            1,
            ProbeFailure::Timeout,
        )
        .await;
        assert_eq!(r.status, CheckStatus::Error);
        assert!(r.error.as_deref().unwrap().contains("timeout"));
    }

    #[tokio::test]
    async fn no_state_emits_real_error() {
        let target = Uuid::new_v4();
        let org = Uuid::new_v4();
        let store: Arc<InMemoryDomainExpiryStateStore> =
            Arc::new(InMemoryDomainExpiryStateStore::new());
        let runtime = DomainExpiryRuntime::new(
            Arc::new(RegistrationClient::new(Arc::new(RdapClient::new(
                crate::http_outbound::build_outbound_client(crate::security::SsrfGuard::strict()),
            )))),
            Arc::new(RdapSingleflight::with_default_ttl()),
            store,
            HostThrottle::permissive(),
            StdDuration::from_secs(7 * 24 * 3600),
        );
        let r = fall_back(
            target,
            org,
            &check("example.com"),
            &runtime,
            Utc::now(),
            1,
            ProbeFailure::Lookup(anyhow!("nxdomain")),
        )
        .await;
        assert_eq!(r.status, CheckStatus::Error);
        assert!(r.error.as_deref().unwrap().contains("nxdomain"));
    }
}
