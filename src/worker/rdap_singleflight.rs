//! Singleflight + short-TTL cache for RDAP probes, keyed by canonical
//! domain. RDAP responses are public registry data — coalescing across
//! tenants is safe and drops outbound traffic by ~100× when many customers
//! monitor the same popular domains.
//!
//! Semantics:
//!  - First caller for `domain` acquires the per-slot mutex and runs the
//!    fetcher closure.
//!  - Concurrent callers for the same `domain` wait on the same mutex; when
//!    the first caller fills the slot, the rest read a `Ready` value
//!    without invoking the closure.
//!  - A successful response is reused for `cache_ttl` (default 60s). The
//!    window is short by design: durable last-good lives in Postgres
//!    `domain_expiry_state`; the in-process cache only absorbs
//!    scheduler-stagger waves and concurrent target dispatch.
//!  - Errors are *not* cached. The next caller after a failure tries again,
//!    so a transient blip doesn't poison the slot.

use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

use dashmap::DashMap;
use tokio::sync::Mutex;

use crate::error::Result;
use crate::worker::registration::types::RegistrationAnswer;
use crate::worker::sweep::sweep_idle;

/// Default in-process cache window for a successful RDAP answer.
pub const DEFAULT_CACHE_TTL: Duration = Duration::from_secs(60);

pub struct RdapSingleflight {
    slots: DashMap<Arc<str>, Arc<Slot>>,
    cache_ttl: Duration,
}

struct Slot {
    state: Mutex<SlotState>,
}

enum SlotState {
    Empty,
    Ready {
        value: Arc<RegistrationAnswer>,
        fetched_at: Instant,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchOutcome {
    Hit,
    Miss,
}

impl RdapSingleflight {
    pub fn new(cache_ttl: Duration) -> Self {
        Self {
            slots: DashMap::new(),
            cache_ttl,
        }
    }

    pub fn with_default_ttl() -> Self {
        Self::new(DEFAULT_CACHE_TTL)
    }

    /// Calls `fetcher` only when the slot for `domain` is missing or stale;
    /// concurrent callers for the same key collapse to one invocation.
    pub async fn lookup<F, Fut>(
        &self,
        domain: Arc<str>,
        fetcher: F,
    ) -> Result<(Arc<RegistrationAnswer>, FetchOutcome)>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<RegistrationAnswer>>,
    {
        let slot = self
            .slots
            .entry(domain.clone())
            .or_insert_with(|| {
                Arc::new(Slot {
                    state: Mutex::new(SlotState::Empty),
                })
            })
            .clone();

        // MUST stay `tokio::sync::Mutex`. The guard is held across
        // `fetcher().await` to serialise concurrent callers onto one
        // outbound RDAP request — a sync mutex (parking_lot) would block the
        // worker thread for the full RDAP round-trip and saturate the
        // runtime under contention.
        let mut guard = slot.state.lock().await;
        if let SlotState::Ready { value, fetched_at } = &*guard
            && fetched_at.elapsed() < self.cache_ttl
        {
            return Ok((value.clone(), FetchOutcome::Hit));
        }

        let answer = fetcher().await?;
        let value = Arc::new(answer);
        *guard = SlotState::Ready {
            value: value.clone(),
            fetched_at: Instant::now(),
        };
        Ok((value, FetchOutcome::Miss))
    }

    pub fn len(&self) -> usize {
        self.slots.len()
    }

    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// Off-hot-path eviction. Drops slots that are no longer useful:
    ///  - `Ready` slots whose `fetched_at` is older than `cache_ttl` (no
    ///    cache hit possible — next call would refetch anyway).
    ///  - `Empty` slots left behind by a fetcher failure.
    ///
    /// Skips slots a live caller still references (`strong_count > 1` —
    /// handled by the shared helper) and slots whose state is currently
    /// locked (`try_lock` fails). Atomic per shard.
    pub fn sweep(&self) -> usize {
        let cache_ttl = self.cache_ttl;
        sweep_idle(&self.slots, |slot| {
            let Ok(guard) = slot.state.try_lock() else {
                return false;
            };
            match &*guard {
                SlotState::Empty => true,
                SlotState::Ready { fetched_at, .. } => fetched_at.elapsed() >= cache_ttl,
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn answer() -> RegistrationAnswer {
        RegistrationAnswer {
            expiration: Utc::now() + chrono::Duration::days(90),
            registrar: Some("Test Registrar".into()),
        }
    }

    #[tokio::test]
    async fn concurrent_callers_collapse_to_one_fetch() {
        let sf = Arc::new(RdapSingleflight::new(Duration::from_secs(60)));
        let calls = Arc::new(AtomicUsize::new(0));
        let domain: Arc<str> = Arc::from("example.com");

        let mut handles = Vec::new();
        for _ in 0..10 {
            let sf = sf.clone();
            let calls = calls.clone();
            let domain = domain.clone();
            handles.push(tokio::spawn(async move {
                sf.lookup(domain, || async {
                    calls.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(30)).await;
                    Ok(answer())
                })
                .await
                .unwrap()
            }));
        }
        let mut hits = 0;
        let mut misses = 0;
        for h in handles {
            let (_, outcome) = h.await.unwrap();
            match outcome {
                FetchOutcome::Hit => hits += 1,
                FetchOutcome::Miss => misses += 1,
            }
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(misses, 1);
        assert_eq!(hits, 9);
    }

    #[tokio::test]
    async fn ttl_expiry_triggers_refetch() {
        let sf = RdapSingleflight::new(Duration::from_millis(20));
        let calls = Arc::new(AtomicUsize::new(0));
        let domain: Arc<str> = Arc::from("example.com");

        let _ = sf
            .lookup(domain.clone(), || async {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(answer())
            })
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;
        let (_, outcome) = sf
            .lookup(domain, || async {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(answer())
            })
            .await
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(outcome, FetchOutcome::Miss);
    }

    #[tokio::test]
    async fn failure_is_not_cached() {
        let sf = RdapSingleflight::new(Duration::from_secs(60));
        let domain: Arc<str> = Arc::from("example.com");
        let err = sf
            .lookup(domain.clone(), || async {
                Err::<RegistrationAnswer, _>(crate::error::AppError::Other(anyhow::anyhow!("boom")))
            })
            .await;
        assert!(err.is_err());
        // Second call must invoke the fetcher again because the previous
        // failure is not cached.
        let calls = Arc::new(AtomicUsize::new(0));
        let (_, outcome) = sf
            .lookup(domain, || {
                let calls = calls.clone();
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Ok(answer())
                }
            })
            .await
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(outcome, FetchOutcome::Miss);
    }

    /// Singleflight key normalisation lives in `domain_expiry::fresh_probe`
    /// — by the time a domain hits `lookup` it is the canonical
    /// (lowercased + IDN-encoded + trailing-dot stripped) form. This test
    /// asserts the cache contract on the canonical key: identical keys
    /// share a slot regardless of how many `Arc<str>` clones the caller
    /// makes.
    #[tokio::test]
    async fn canonical_keys_share_one_slot() {
        let sf = Arc::new(RdapSingleflight::new(Duration::from_secs(60)));
        let calls = Arc::new(AtomicUsize::new(0));
        for _ in 0..3 {
            let _ = sf
                .lookup(Arc::<str>::from("xn--bhn-qla.de"), || {
                    let calls = calls.clone();
                    async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        Ok(answer())
                    }
                })
                .await
                .unwrap();
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(sf.len(), 1);
    }

    #[tokio::test]
    async fn sweep_drops_stale_ready_slots() {
        let sf = RdapSingleflight::new(Duration::from_millis(20));
        let calls = Arc::new(AtomicUsize::new(0));
        let _ = sf
            .lookup(Arc::<str>::from("example.com"), || {
                let calls = calls.clone();
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Ok(answer())
                }
            })
            .await
            .unwrap();
        assert_eq!(sf.len(), 1);
        // Inside TTL: sweep keeps it.
        assert_eq!(sf.sweep(), 0);
        assert_eq!(sf.len(), 1);
        tokio::time::sleep(Duration::from_millis(30)).await;
        // Past TTL: sweep drops it.
        assert_eq!(sf.sweep(), 1);
        assert_eq!(sf.len(), 0);
    }

    #[tokio::test]
    async fn sweep_drops_empty_slot_after_failure() {
        let sf = RdapSingleflight::new(Duration::from_secs(60));
        let err = sf
            .lookup(Arc::<str>::from("example.com"), || async {
                Err::<RegistrationAnswer, _>(crate::error::AppError::Other(anyhow::anyhow!("boom")))
            })
            .await;
        assert!(err.is_err());
        // Failure leaves the slot allocated but `Empty`. Sweep must reclaim
        // it so a long tail of failed lookups can't grow the map unbounded.
        assert_eq!(sf.len(), 1);
        assert_eq!(sf.sweep(), 1);
        assert_eq!(sf.len(), 0);
    }

    #[tokio::test]
    async fn sweep_keeps_slot_with_live_external_arc() {
        // Isolates the strong_count guard: caller has cloned the slot's Arc
        // out of the map but holds NO lock on it. Past TTL the try_lock check
        // alone would evict; only the strong_count guard saves it.
        let sf = RdapSingleflight::new(Duration::from_millis(20));
        let _ = sf
            .lookup(Arc::<str>::from("example.com"), || async { Ok(answer()) })
            .await
            .unwrap();
        let stash: Arc<Slot> = sf
            .slots
            .get("example.com")
            .expect("slot present")
            .value()
            .clone();
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(sf.sweep(), 0, "external Arc clone must protect the slot");
        assert_eq!(sf.len(), 1);
        drop(stash);
        assert_eq!(sf.sweep(), 1, "released external clone — sweep reclaims");
        assert_eq!(sf.len(), 0);
    }

    #[tokio::test]
    async fn sweep_keeps_in_flight_slot() {
        let sf = Arc::new(RdapSingleflight::new(Duration::from_millis(20)));
        let started = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let started_c = started.clone();
        let release_c = release.clone();
        let sf_c = sf.clone();
        let task = tokio::spawn(async move {
            sf_c.lookup(Arc::<str>::from("example.com"), || async move {
                started_c.notify_one();
                release_c.notified().await;
                Ok(answer())
            })
            .await
            .unwrap()
        });
        started.notified().await;
        // Fetcher holds the slot's mutex AND keeps the Arc alive — sweep
        // must skip it even past TTL, otherwise the in-flight result is
        // discarded.
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(sf.sweep(), 0);
        assert_eq!(sf.len(), 1);
        release.notify_one();
        let _ = task.await.unwrap();
    }

    #[tokio::test]
    async fn distinct_domains_each_fetch_once() {
        let sf = RdapSingleflight::new(Duration::from_secs(60));
        let calls = Arc::new(AtomicUsize::new(0));
        for name in ["a.com", "b.com", "c.com"] {
            let _ = sf
                .lookup(Arc::from(name), || {
                    let calls = calls.clone();
                    async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        Ok(answer())
                    }
                })
                .await
                .unwrap();
        }
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        assert_eq!(sf.len(), 3);
    }
}
