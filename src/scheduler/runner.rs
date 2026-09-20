use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use metrics::{Counter, Gauge, counter, gauge, histogram};
use tokio::sync::mpsc;
use tokio::time::{Instant, MissedTickBehavior};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::config::SchedulerConfig;
use crate::error::Result;
use crate::metric_names;
use crate::scheduler::registry::{RegistryDiff, ScheduledTarget, TargetRegistry};
use crate::worker::{CheckTask, WorkerPool};

/// Fixed sweep cadence — decoupled from registry-refresh so a DB outage or
/// an operator-tuned refresh interval doesn't delay memory reclamation.
const SWEEP_INTERVAL: Duration = Duration::from_secs(30);

/// Backoff cap on consecutive refresh failures: next attempt waits at most
/// `base × REFRESH_BACKOFF_CAP_MULTIPLIER` seconds. Bounds recovery latency
/// when the upstream DB returns — at default base=30s and cap=10, steady-state
/// retry cadence under sustained outage is 5 minutes, with recovery noticed
/// within the same window.
const REFRESH_BACKOFF_CAP_MULTIPLIER: u64 = 10;

/// First-probe stagger cap: bounds the boot/refresh-batch burst while
/// keeping a freshly-added target's first result within a few seconds.
const INITIAL_PROBE_SPREAD: Duration = Duration::from_secs(5);

/// Diff backlog tolerated before the refresher back-pressures.
const REFRESH_CHANNEL_BOUND: usize = 8;

static REFRESH_FAILED: LazyLock<Counter> =
    LazyLock::new(|| counter!(metric_names::SCHEDULER_REFRESH_FAILED));
static CONSECUTIVE_REFRESH_FAILURES: LazyLock<Gauge> =
    LazyLock::new(|| gauge!(metric_names::SCHEDULER_CONSECUTIVE_REFRESH_FAILURES));

pub struct Scheduler {
    registry: Arc<TargetRegistry>,
    pool: Arc<WorkerPool>,
    cfg: SchedulerConfig,
    scheduled: Arc<AtomicUsize>,
}

/// A pending fire. `first` marks the initial staggered probe; `seq` is the
/// schedule generation — an entry is live only while it matches the id's
/// current `seq`, so a re-scheduled id's older entries drop on pop.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Due {
    at: Instant,
    id: Uuid,
    seq: u64,
    first: bool,
}

impl Scheduler {
    pub fn new(registry: Arc<TargetRegistry>, pool: Arc<WorkerPool>, cfg: SchedulerConfig) -> Self {
        Self {
            registry,
            pool,
            cfg,
            scheduled: Arc::new(AtomicUsize::new(0)),
        }
    }

    pub fn active_tasks(&self) -> usize {
        self.scheduled.load(Ordering::Relaxed)
    }

    pub async fn run(self: Arc<Self>, shutdown: CancellationToken) -> Result<()> {
        debug_assert!(
            self.cfg.target_refresh_interval_secs >= 1,
            "target_refresh_interval_secs must be validated >= 1 before Scheduler::run",
        );

        // Refresh runs off the driver so a slow PG round-trip can never stall
        // probe dispatch — the driver only ever does in-memory work.
        let (diff_tx, mut diff_rx) = mpsc::channel::<RegistryDiff>(REFRESH_CHANNEL_BOUND);
        let refresher = tokio::spawn(Arc::clone(&self).run_refresher(diff_tx, shutdown.clone()));

        let mut sweep = tokio::time::interval(SWEEP_INTERVAL);
        sweep.set_missed_tick_behavior(MissedTickBehavior::Skip);

        let mut heap: BinaryHeap<Reverse<Due>> = BinaryHeap::new();
        let mut live: HashMap<Uuid, u64> = HashMap::new();
        let mut next_seq: u64 = 0;

        loop {
            let next_at = heap.peek().map(|Reverse(due)| due.at);
            tokio::select! {
                _ = shutdown.cancelled() => break,
                maybe = diff_rx.recv() => match maybe {
                    Some(diff) => self.apply_diff(&diff, &mut heap, &mut live, &mut next_seq),
                    None => break,
                },
                _ = sweep.tick() => self.sweep_once(),
                _ = sleep_until_opt(next_at) => self.drain_due(&mut heap, &mut live),
            }
        }

        drop(diff_rx);
        let _ = refresher.await;
        Ok(())
    }

    async fn run_refresher(
        self: Arc<Self>,
        diff_tx: mpsc::Sender<RegistryDiff>,
        shutdown: CancellationToken,
    ) {
        let mut refresh =
            tokio::time::interval(Duration::from_secs(self.cfg.target_refresh_interval_secs));
        refresh.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let mut consecutive_failures: u32 = 0;

        loop {
            tokio::select! {
                _ = shutdown.cancelled() => return,
                _ = refresh.tick() => {
                    let outcome = tokio::select! {
                        _ = shutdown.cancelled() => return,
                        r = self.refresh_registry() => r,
                    };
                    match outcome {
                        Ok(diff) => {
                            self.handle_refresh_result(&mut consecutive_failures, &mut refresh, Ok(()));
                            if diff_tx.send(diff).await.is_err() {
                                return;
                            }
                        }
                        Err(err) => {
                            self.handle_refresh_result(&mut consecutive_failures, &mut refresh, Err(err));
                        }
                    }
                }
            }
        }
    }

    async fn refresh_registry(&self) -> Result<RegistryDiff> {
        let started = std::time::Instant::now();
        let result = self.registry.refresh().await;
        histogram!(metric_names::SCHEDULER_REFRESH_DURATION_MS)
            .record(started.elapsed().as_millis() as f64);
        result
    }

    fn handle_refresh_result(
        &self,
        consecutive_failures: &mut u32,
        refresh: &mut tokio::time::Interval,
        result: Result<()>,
    ) {
        match result {
            Ok(()) => {
                if *consecutive_failures > 0 {
                    tracing::info!(
                        prior_consecutive_failures = *consecutive_failures,
                        "registry refresh recovered",
                    );
                    CONSECUTIVE_REFRESH_FAILURES.set(0.0);
                    *consecutive_failures = 0;
                }
            }
            Err(err) => {
                *consecutive_failures = consecutive_failures.saturating_add(1);
                REFRESH_FAILED.increment(1);
                CONSECUTIVE_REFRESH_FAILURES.set(*consecutive_failures as f64);
                let base = self.cfg.target_refresh_interval_secs;
                let delay_secs = backoff_delay_secs(base, *consecutive_failures);
                tracing::error!(
                    ?err,
                    consecutive_failures = *consecutive_failures,
                    next_attempt_in_secs = delay_secs,
                    "registry refresh failed",
                );
                refresh.reset_after(Duration::from_secs(delay_secs));
            }
        }
    }

    /// Added and updated targets are (re)scheduled with a fresh generation, so
    /// any prior entry for the same id is superseded; removed ids drop from the
    /// live map, leaving their stale entries to reap on pop.
    fn apply_diff(
        &self,
        diff: &RegistryDiff,
        heap: &mut BinaryHeap<Reverse<Due>>,
        live: &mut HashMap<Uuid, u64>,
        next_seq: &mut u64,
    ) {
        let now = Instant::now();
        for st in diff.added.iter().chain(diff.updated.iter()) {
            schedule(st, heap, live, next_seq, now);
        }
        for id in &diff.removed {
            live.remove(id);
        }
        self.scheduled.store(live.len(), Ordering::Relaxed);
    }

    fn drain_due(&self, heap: &mut BinaryHeap<Reverse<Due>>, live: &mut HashMap<Uuid, u64>) {
        let now = Instant::now();
        loop {
            match heap.peek() {
                Some(Reverse(due)) if due.at <= now => {}
                _ => break,
            }
            let Reverse(due) = heap.pop().expect("peeked a due entry");
            if live.get(&due.id) != Some(&due.seq) {
                continue;
            }
            let Some(st) = self.registry.get(&due.id) else {
                live.remove(&due.id);
                continue;
            };
            let base = st.target.interval;
            let anchor = if due.first {
                due.at + stagger_offset(due.id, base)
            } else {
                due.at
            };
            heap.push(Reverse(Due {
                at: next_tick(anchor, base, now),
                id: due.id,
                seq: due.seq,
                first: false,
            }));
            dispatch(&self.pool, st);
        }
        self.scheduled.store(live.len(), Ordering::Relaxed);
    }

    fn sweep_once(&self) {
        let evicted_throttle = self.pool.host_throttle().sweep();
        let evicted_breakers = self.pool.sweep_breakers();
        let evicted_singleflight = self.pool.domain_expiry_runtime().singleflight.sweep();
        if evicted_throttle > 0 || evicted_breakers > 0 || evicted_singleflight > 0 {
            tracing::debug!(
                evicted_throttle,
                evicted_breakers,
                evicted_singleflight,
                "scheduler swept idle entries"
            );
        }
    }
}

/// Schedule a target's first probe within the initial-spread cap under a fresh
/// monotonic generation, so any in-flight entry for the same id is superseded.
fn schedule(
    st: &ScheduledTarget,
    heap: &mut BinaryHeap<Reverse<Due>>,
    live: &mut HashMap<Uuid, u64>,
    next_seq: &mut u64,
    now: Instant,
) {
    let id = st.target.id;
    let seq = *next_seq;
    *next_seq = next_seq.wrapping_add(1);
    live.insert(id, seq);
    let base = st.target.interval;
    debug_assert!(!base.is_zero(), "target interval must be non-zero");
    let cap = INITIAL_PROBE_SPREAD.min(base / 2);
    heap.push(Reverse(Due {
        at: now + stagger_offset(id, cap),
        id,
        seq,
        first: true,
    }));
}

/// First phase-aligned grid point strictly after `now` — the heap form of
/// `MissedTickBehavior::Skip`, dropping any backlog while preserving phase.
fn next_tick(sched: Instant, base: Duration, now: Instant) -> Instant {
    let next = sched + base;
    if next > now {
        return next;
    }
    let base_ns = base.as_nanos().max(1);
    let behind_ns = now.saturating_duration_since(next).as_nanos();
    let skips = ((behind_ns / base_ns) + 1).min(u32::MAX as u128) as u32;
    next + base * skips
}

async fn sleep_until_opt(at: Option<Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending::<()>().await,
    }
}

/// Exponential backoff for consecutive refresh failures. Doubles each
/// failure until it hits the cap (`base × REFRESH_BACKOFF_CAP_MULTIPLIER`)
/// — bounded so the scheduler notices PG recovery within one cap-window.
fn backoff_delay_secs(base_secs: u64, consecutive_failures: u32) -> u64 {
    let shift = consecutive_failures.saturating_sub(1).min(u32::BITS - 1);
    let mult = 1u64.checked_shl(shift).unwrap_or(u64::MAX);
    let mult = mult.min(REFRESH_BACKOFF_CAP_MULTIPLIER);
    base_secs.saturating_mul(mult)
}

fn dispatch(pool: &WorkerPool, st: ScheduledTarget) {
    pool.dispatch(CheckTask {
        target: st.target,
        org_id: st.org_id,
        host_key: st.host_key,
        breaker_key: st.breaker_key,
        rdap_tld: st.rdap_tld,
    });
}

fn stagger_offset(id: Uuid, interval: Duration) -> Duration {
    let interval_ms = interval.as_millis() as u64;
    if interval_ms == 0 {
        return Duration::ZERO;
    }
    let mut h = DefaultHasher::new();
    id.hash(&mut h);
    Duration::from_millis(h.finish() % interval_ms)
}

#[cfg(test)]
mod tests {
    use super::{
        Due, REFRESH_BACKOFF_CAP_MULTIPLIER, backoff_delay_secs, next_tick, stagger_offset,
    };
    use std::cmp::Reverse;
    use std::collections::BinaryHeap;
    use std::time::Duration;
    use tokio::time::Instant;
    use uuid::Uuid;

    #[test]
    fn stagger_offset_is_deterministic() {
        let id = Uuid::from_u128(0x1234_5678_90AB_CDEF_1234_5678_90AB_CDEF);
        let base = Duration::from_secs(60);
        let a = stagger_offset(id, base);
        let b = stagger_offset(id, base);
        assert_eq!(a, b, "same id+interval must yield identical offset");
    }

    #[test]
    fn stagger_offset_stays_within_interval() {
        let base = Duration::from_secs(60);
        for n in 0..1000u128 {
            let id = Uuid::from_u128(n);
            let o = stagger_offset(id, base);
            assert!(o < base, "offset {o:?} must be < interval {base:?}");
        }
    }

    #[test]
    fn stagger_offset_zero_interval_is_safe() {
        assert_eq!(stagger_offset(Uuid::nil(), Duration::ZERO), Duration::ZERO);
    }

    #[test]
    fn initial_offset_is_bounded_by_spread_cap() {
        use super::INITIAL_PROBE_SPREAD;
        let long_base = Duration::from_secs(3600);
        for n in 0..1000u128 {
            let cap = INITIAL_PROBE_SPREAD.min(long_base / 2);
            let o = stagger_offset(Uuid::from_u128(n), cap);
            assert!(
                o < INITIAL_PROBE_SPREAD,
                "first-probe offset {o:?} exceeded cap"
            );
        }
        let short_base = Duration::from_secs(2);
        for n in 0..1000u128 {
            let cap = INITIAL_PROBE_SPREAD.min(short_base / 2);
            let o = stagger_offset(Uuid::from_u128(n), cap);
            assert!(
                o < short_base,
                "first-probe offset {o:?} >= interval {short_base:?}"
            );
        }
    }

    #[test]
    fn stagger_offset_distributes_uniformly() {
        let base = Duration::from_secs(60);
        let bucket_ms = base.as_millis() as u64 / 10;
        let mut buckets = [0u32; 10];
        for n in 0..1000u128 {
            let o = stagger_offset(Uuid::from_u128(n + 0xDEAD_BEEF_0000), base);
            let idx = (o.as_millis() as u64 / bucket_ms).min(9) as usize;
            buckets[idx] += 1;
        }
        for (i, &c) in buckets.iter().enumerate() {
            assert!(
                (60..=160).contains(&c),
                "bucket {i} got {c} hits — distribution is clumped: {buckets:?}",
            );
        }
    }

    #[tokio::test]
    async fn next_tick_advances_one_interval_on_time() {
        let base = Duration::from_secs(10);
        let sched = Instant::now();
        let now = sched + Duration::from_millis(1);
        assert_eq!(next_tick(sched, base, now), sched + base);
    }

    #[tokio::test]
    async fn next_tick_skips_backlog_when_behind() {
        let base = Duration::from_secs(10);
        let sched = Instant::now();
        let now = sched + base + base * 2 + base / 2;
        let next = next_tick(sched, base, now);
        assert!(next > now, "next {next:?} must be strictly future");
        assert_eq!(next, sched + base * 4);
    }

    #[tokio::test]
    async fn heap_pops_earliest_due_first() {
        let base = Instant::now();
        let mut heap: BinaryHeap<Reverse<Due>> = BinaryHeap::new();
        for secs in [5u64, 1, 3] {
            heap.push(Reverse(Due {
                at: base + Duration::from_secs(secs),
                id: Uuid::from_u128(secs as u128),
                seq: 0,
                first: false,
            }));
        }
        let order: Vec<u64> = std::iter::from_fn(|| heap.pop())
            .map(|Reverse(d)| (d.at - base).as_secs())
            .collect();
        assert_eq!(order, vec![1, 3, 5]);
    }

    #[test]
    fn backoff_walks_the_expected_curve_with_default_base() {
        let base = 30u64;
        assert_eq!(backoff_delay_secs(base, 1), 30);
        assert_eq!(backoff_delay_secs(base, 2), 60);
        assert_eq!(backoff_delay_secs(base, 3), 120);
        assert_eq!(backoff_delay_secs(base, 4), 240);
        assert_eq!(
            backoff_delay_secs(base, 5),
            base * REFRESH_BACKOFF_CAP_MULTIPLIER
        );
        assert_eq!(
            backoff_delay_secs(base, 100),
            base * REFRESH_BACKOFF_CAP_MULTIPLIER
        );
    }

    #[test]
    fn backoff_does_not_panic_at_extremes() {
        assert!(backoff_delay_secs(u64::MAX, 1) >= 1);
        assert!(backoff_delay_secs(u64::MAX, u32::MAX) >= 1);
        assert_eq!(backoff_delay_secs(1, 0), 1);
    }
}
