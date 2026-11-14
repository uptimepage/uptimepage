//! What each target's window held at the last pass, so a pass reads only what
//! has arrived since. Rows only, never decisions: every pass still decides
//! from scratch over the rows in view.

use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant};

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use parking_lot::Mutex;
use uuid::Uuid;

use crate::domain::CheckOutcome;
use crate::storage::RegionOutcome;

/// A target's outcomes per region, oldest first.
pub(super) type ByRegion = BTreeMap<String, Vec<CheckOutcome>>;

struct Arrived {
    arrival: u64,
    outcome: CheckOutcome,
}

struct Held {
    by_region: BTreeMap<String, Vec<Arrived>>,
    /// The window the rows cover. A longer one was never read.
    lookback: ChronoDuration,
    /// When the last read began; anything that arrived later is unread.
    read_at: Instant,
    whole_read_at: Instant,
}

#[derive(Default)]
pub(super) struct Recent {
    held: Mutex<HashMap<Uuid, Held>>,
}

/// One read of a group of targets.
#[derive(Debug, Clone, Copy)]
pub(super) struct Read {
    pub whole: bool,
    pub at: Instant,
    /// The pass's clock, which each target's window is cut by.
    pub now: DateTime<Utc>,
}

impl Recent {
    /// When `target`'s last read began, or `None` when its window has to be
    /// read whole: never read, grown past what is held, or due the reload
    /// that picks up whatever a pass missed.
    pub(super) fn last_read(
        &self,
        target: Uuid,
        lookback: ChronoDuration,
        reload_every: Duration,
    ) -> Option<Instant> {
        let held = self.held.lock();
        let entry = held.get(&target)?;
        (entry.lookback >= lookback && entry.whole_read_at.elapsed() < reload_every)
            .then_some(entry.read_at)
    }

    /// Take in a read of `targets`, each with its own lookback. A whole read
    /// replaces what was held. Otherwise the store sent everything from some
    /// arrival onward, so what is held from that arrival on is replaced too:
    /// a row read twice is held once, and two rows a second holds are both
    /// kept. A target that returned nothing still has its window moved on.
    pub(super) fn absorb(
        &self,
        targets: &[(Uuid, ChronoDuration)],
        rows: Vec<RegionOutcome>,
        read: Read,
        per_region_cap: usize,
    ) {
        let mut arrived: HashMap<Uuid, BTreeMap<String, Vec<Arrived>>> = HashMap::new();
        for row in rows {
            arrived
                .entry(row.outcome.target_id)
                .or_default()
                .entry(row.region)
                .or_default()
                .push(Arrived {
                    arrival: row.arrival,
                    outcome: row.outcome,
                });
        }

        let mut held = self.held.lock();
        for (target, lookback) in targets {
            if read.whole {
                held.insert(
                    *target,
                    Held {
                        by_region: BTreeMap::new(),
                        lookback: *lookback,
                        read_at: read.at,
                        whole_read_at: read.at,
                    },
                );
            }
            let Some(entry) = held.get_mut(target) else {
                continue;
            };
            entry.lookback = *lookback;
            entry.read_at = read.at;
            for (region, mut rows) in arrived.remove(target).unwrap_or_default() {
                let kept = entry.by_region.entry(region).or_default();
                if let Some(resent_from) = rows.iter().map(|row| row.arrival).min() {
                    kept.retain(|row| row.arrival < resent_from);
                }
                kept.append(&mut rows);
            }
            let window_start = read.now - *lookback;
            for rows in entry.by_region.values_mut() {
                settle(rows, window_start, per_region_cap);
            }
            entry.by_region.retain(|_, rows| !rows.is_empty());
        }
    }

    pub(super) fn windows(&self, targets: impl Iterator<Item = Uuid>) -> HashMap<Uuid, ByRegion> {
        let held = self.held.lock();
        targets
            .filter_map(|target| {
                let by_region = held
                    .get(&target)?
                    .by_region
                    .iter()
                    .map(|(region, rows)| {
                        let outcomes = rows.iter().map(|row| row.outcome.clone()).collect();
                        (region.clone(), outcomes)
                    })
                    .collect();
                Some((target, by_region))
            })
            .collect()
    }

    /// Drop targets no pass has read for `idle`: disabled, deleted, or stale
    /// enough to need a whole read anyway.
    pub(super) fn forget_idle(&self, idle: Duration) {
        self.held
            .lock()
            .retain(|_, entry| entry.read_at.elapsed() < idle);
    }
}

/// Oldest first, inside the window and no more than the newest `cap`. Rows
/// of one second go in the order they arrived, and rows that arrived together
/// worst last, so a tie never hides a failure and reads the same every pass.
fn settle(rows: &mut Vec<Arrived>, window_start: DateTime<Utc>, cap: usize) {
    rows.sort_by_key(|row| {
        (
            row.outcome.timestamp,
            row.arrival,
            row.outcome.status.severity_rank(),
        )
    });
    let expired = rows.partition_point(|row| row.outcome.timestamp < window_start);
    let over = rows.len().saturating_sub(cap);
    rows.drain(..expired.max(over));
}
