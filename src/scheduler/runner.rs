use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use metrics::{Counter, Gauge, counter, gauge, histogram};
use tokio::sync::mpsc;
use tokio::time::{Instant, MissedTickBehavior};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::config::SchedulerConfig;
use crate::domain::Target;
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

/// A target someone is waiting on gets its first result within this, or within
/// half its interval when that is shorter.
const INITIAL_PROBE_SPREAD: Duration = Duration::from_secs(5);

/// A target that first appears this soon after its last change was just
/// created or resumed. Several refresh intervals wide, so an agent that pulls
/// late still counts it.
const RECENT_EDIT: Duration = Duration::from_secs(300);

/// A target whose slot is further off than this after start gets its first
/// result within it instead, spread so the start is still no burst.
const STARTUP_SPREAD: Duration = Duration::from_secs(300);

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

/// A pending fire. `seq` is the schedule generation — an entry is live only
/// while it matches the id's current `seq`, so a re-scheduled id's older
/// entries drop on pop.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Due {
    at: Instant,
    id: Uuid,
    seq: u64,
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
                _ = sweep.tick() => self.sweep_once(&live),
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
    /// live map, leaving their stale entries to reap on pop. An updated target
    /// was edited or moved by its plan, so its new config probes within
    /// seconds, on a fresh circuit breaker when what it probes changed.
    fn apply_diff(
        &self,
        diff: &RegistryDiff,
        heap: &mut BinaryHeap<Reverse<Due>>,
        live: &mut HashMap<Uuid, u64>,
        next_seq: &mut u64,
    ) {
        let now = Instant::now();
        let wall = SystemTime::now();
        let region = &self.cfg.region;
        for st in &diff.added {
            let wait = first_probe_in(&st.target, region, wall);
            schedule(st.target.id, heap, live, next_seq, now + wait);
        }
        for st in &diff.updated {
            let wait = quick_probe_in(&st.target, region);
            schedule(st.target.id, heap, live, next_seq, now + wait);
        }
        for id in &diff.probe_changed {
            self.pool.forget_breaker(*id);
        }
        for id in &diff.removed {
            live.remove(id);
        }
        self.scheduled.store(live.len(), Ordering::Relaxed);
    }

    fn drain_due(&self, heap: &mut BinaryHeap<Reverse<Due>>, live: &mut HashMap<Uuid, u64>) {
        let now = Instant::now();
        let wall = SystemTime::now();
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
            heap.push(Reverse(Due {
                at: now + until_slot(due.id, &self.cfg.region, base, wall, base / 2),
                id: due.id,
                seq: due.seq,
            }));
            dispatch(&self.pool, st);
        }
        self.scheduled.store(live.len(), Ordering::Relaxed);
    }

    fn sweep_once(&self, live: &HashMap<Uuid, u64>) {
        let evicted_throttle = self.pool.host_throttle().sweep();
        let evicted_breakers = self.pool.sweep_breakers(|id| live.contains_key(id));
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

/// Schedule a target's first probe under a fresh monotonic generation, so any
/// in-flight entry for the same id is superseded.
fn schedule(
    id: Uuid,
    heap: &mut BinaryHeap<Reverse<Due>>,
    live: &mut HashMap<Uuid, u64>,
    next_seq: &mut u64,
    at: Instant,
) {
    let seq = *next_seq;
    *next_seq = next_seq.wrapping_add(1);
    live.insert(id, seq);
    heap.push(Reverse(Due { at, id, seq }));
}

/// A target this process has not scheduled yet probes within seconds when it
/// was just created or resumed. Otherwise it starts on its slot, or within
/// `STARTUP_SPREAD` when the slot is further off.
fn first_probe_in(target: &Target, region: &str, now: SystemTime) -> Duration {
    let changed = now
        .duration_since(target.updated_at.into())
        .unwrap_or_default();
    if changed < RECENT_EDIT {
        return quick_probe_in(target, region);
    }
    let slot = until_slot(target.id, region, target.interval, now, Duration::ZERO);
    if slot <= STARTUP_SPREAD {
        slot
    } else {
        stagger_offset(target.id, region, STARTUP_SPREAD)
    }
}

fn quick_probe_in(target: &Target, region: &str) -> Duration {
    debug_assert!(
        !target.interval.is_zero(),
        "target interval must be non-zero"
    );
    stagger_offset(
        target.id,
        region,
        INITIAL_PROBE_SPREAD.min(target.interval / 2),
    )
}

/// Time from `now` to the target's first slot at least `gap` away. Slots sit
/// at a fixed offset per target and region on a wall-clock grid of the
/// interval, so a restart resumes the rhythm of the process it replaced
/// instead of probing every target at once. Regions keep their own offsets,
/// so a blip shorter than the interval is not seen by all of them at once.
fn until_slot(
    id: Uuid,
    region: &str,
    interval: Duration,
    now: SystemTime,
    gap: Duration,
) -> Duration {
    let interval_ms = interval.as_millis().max(1);
    let phase_ms = stagger_offset(id, region, interval).as_millis();
    let from_ms = now
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .saturating_add(gap)
        .as_millis();
    let into_cycle = (from_ms + interval_ms - phase_ms) % interval_ms;
    let wait_ms = (interval_ms - into_cycle) % interval_ms;
    gap + Duration::from_millis(wait_ms as u64)
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
        rdap_tld: st.rdap_tld,
    });
}

/// FNV-1a over the raw bytes rather than std's hasher, whose algorithm may
/// change with the toolchain and move every slot with it.
fn stagger_offset(id: Uuid, region: &str, interval: Duration) -> Duration {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0100_0000_01b3;
    let interval_ms = interval.as_millis() as u64;
    if interval_ms == 0 {
        return Duration::ZERO;
    }
    let hash = id
        .as_bytes()
        .iter()
        .chain(region.as_bytes())
        .fold(FNV_OFFSET, |h, &b| {
            (h ^ u64::from(b)).wrapping_mul(FNV_PRIME)
        });
    Duration::from_millis(hash % interval_ms)
}

#[cfg(test)]
mod tests {
    use super::{
        Due, INITIAL_PROBE_SPREAD, RECENT_EDIT, REFRESH_BACKOFF_CAP_MULTIPLIER, STARTUP_SPREAD,
        backoff_delay_secs, first_probe_in, quick_probe_in, stagger_offset, until_slot,
    };
    use crate::domain::Target;
    use std::cmp::Reverse;
    use std::collections::BinaryHeap;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};
    use tokio::time::Instant;
    use uuid::Uuid;

    #[test]
    fn stagger_offset_is_deterministic() {
        let id = Uuid::from_u128(0x1234_5678_90AB_CDEF_1234_5678_90AB_CDEF);
        let base = Duration::from_secs(60);
        let a = stagger_offset(id, REGION, base);
        let b = stagger_offset(id, REGION, base);
        assert_eq!(a, b, "same id+interval must yield identical offset");
    }

    #[test]
    fn stagger_offset_is_pinned_so_a_hash_change_moves_no_slot_unnoticed() {
        let id = Uuid::from_u128(0x1234_5678_90AB_CDEF_1234_5678_90AB_CDEF);
        assert_eq!(
            stagger_offset(id, REGION, MINUTE),
            Duration::from_millis(34_055)
        );
    }

    #[test]
    fn stagger_offset_stays_within_interval() {
        let base = Duration::from_secs(60);
        for n in 0..1000u128 {
            let id = Uuid::from_u128(n);
            let o = stagger_offset(id, REGION, base);
            assert!(o < base, "offset {o:?} must be < interval {base:?}");
        }
    }

    #[test]
    fn stagger_offset_zero_interval_is_safe() {
        assert_eq!(
            stagger_offset(Uuid::nil(), REGION, Duration::ZERO),
            Duration::ZERO
        );
    }

    #[test]
    fn stagger_offset_distributes_uniformly() {
        let base = Duration::from_secs(60);
        let bucket_ms = base.as_millis() as u64 / 10;
        let mut buckets = [0u32; 10];
        for n in 0..1000u128 {
            let o = stagger_offset(Uuid::from_u128(n + 0xDEAD_BEEF_0000), REGION, base);
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

    const MINUTE: Duration = Duration::from_secs(60);
    const REGION: &str = "eu-helsinki";

    fn at(secs: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(secs)
    }

    #[test]
    fn a_process_started_any_time_before_a_slot_lands_on_that_same_slot() {
        let id = Uuid::from_u128(7);
        let start = at(1_800_000_000);
        let slot = start + until_slot(id, REGION, MINUTE, start, Duration::ZERO);
        let mut now = start;
        while now <= slot {
            assert_eq!(
                now + until_slot(id, REGION, MINUTE, now, Duration::ZERO),
                slot
            );
            now += Duration::from_millis(3_700);
        }
        assert_eq!(
            now + until_slot(id, REGION, MINUTE, now, Duration::ZERO),
            slot + MINUTE
        );
    }

    #[test]
    fn the_next_slot_keeps_at_least_the_gap() {
        let now = at(1_800_000_123);
        for n in 0..1000u128 {
            let wait = until_slot(Uuid::from_u128(n), REGION, MINUTE, now, MINUTE / 2);
            assert!((MINUTE / 2..MINUTE * 3 / 2).contains(&wait), "{wait:?}");
        }
    }

    #[test]
    fn a_probe_fired_on_its_slot_is_next_due_one_interval_later() {
        let id = Uuid::from_u128(42);
        let start = at(1_800_000_000);
        let slot = start + until_slot(id, REGION, MINUTE, start, Duration::ZERO);
        let late = slot + Duration::from_millis(250);
        assert_eq!(
            late + until_slot(id, REGION, MINUTE, late, MINUTE / 2),
            slot + MINUTE
        );
    }

    const DAY: Duration = Duration::from_secs(86_400);

    const NOW: u64 = 1_800_000_000;

    /// Target `n` of a fleet on `interval`, last edited `edited` before `NOW`.
    fn fleet_target(n: u128, interval: Duration, edited: Duration) -> Target {
        let mut target = Target::sample(interval);
        target.id = Uuid::from_u128(n + 0xFEED_0000);
        target.updated_at = (at(NOW) - edited).into();
        target
    }

    fn first_probes(interval: Duration) -> impl Iterator<Item = (Uuid, Duration)> {
        (0..1000u128).map(move |n| {
            let target = fleet_target(n, interval, DAY);
            (target.id, first_probe_in(&target, REGION, at(NOW)))
        })
    }

    #[test]
    fn targets_loaded_at_start_are_spread_out() {
        for interval in [MINUTE, DAY] {
            let early = first_probes(interval)
                .filter(|(_, wait)| *wait < INITIAL_PROBE_SPREAD)
                .count();
            assert!(
                early < 150,
                "{early} of 1000 every {interval:?} due within 5 s"
            );
        }
    }

    #[test]
    fn a_frequent_target_starts_on_its_slot() {
        for (id, wait) in first_probes(MINUTE) {
            assert_eq!(
                wait,
                until_slot(id, REGION, MINUTE, at(NOW), Duration::ZERO)
            );
        }
    }

    #[test]
    fn a_rarely_checked_target_gets_a_first_result_soon_after_start() {
        for (_, wait) in first_probes(DAY) {
            assert!(wait < STARTUP_SPREAD, "{wait:?}");
        }
    }

    #[test]
    fn regions_probe_a_target_at_their_own_moments() {
        let now = at(1_800_000_000);
        let apart = (0..1000u128)
            .filter(|&n| {
                let id = Uuid::from_u128(n);
                until_slot(id, "eu-helsinki", MINUTE, now, Duration::ZERO)
                    != until_slot(id, "us-east", MINUTE, now, Duration::ZERO)
            })
            .count();
        assert!(apart > 990, "only {apart} of 1000 targets differ");
    }

    #[test]
    fn a_target_just_created_or_resumed_is_probed_within_seconds() {
        for n in 0..1000u128 {
            let target = fleet_target(n, DAY, RECENT_EDIT / 2);
            let wait = first_probe_in(&target, REGION, at(NOW));
            assert!(wait < INITIAL_PROBE_SPREAD, "{wait:?}");
        }
    }

    #[test]
    fn a_quick_probe_lands_within_half_a_short_interval() {
        let interval = Duration::from_secs(2);
        for n in 0..1000u128 {
            let target = fleet_target(n, interval, DAY);
            assert!(quick_probe_in(&target, REGION) < interval / 2);
        }
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
