//! Background task that materialises incidents for every monitor into the
//! Postgres `incidents` table. An incident opens for any enabled target that
//! crosses the failure threshold; whether it is publicly visible is derived at
//! insert time from the monitor's status-page membership.
//!
//! The writer is **purely a follower** of the existing `check_results` stream
//! — it never modifies the hot write path, never gates check execution, and
//! never produces alerts. Its single job is to keep the `incidents` table in
//! sync with what the recent check results say.
//!
//! Detection rule (anything not `up` is unhealthy):
//!  * `≥ flap_threshold` consecutive `down`/`error`/`degraded` results, no open
//!    incident → INSERT a new open incident.
//!  * `≥ flap_threshold` consecutive `up` results while an open incident exists,
//!    held for the monitor's recovery period → UPDATE `ended_at` to the first
//!    such timestamp.
//!
//! Both rules are idempotent: re-running with the same input produces no
//! additional writes. The rules themselves are applied by [`decide_multi`],
//! with no database and no clock; this file is the loop that feeds it and
//! writes what it returns.
//!
//! Each pass decides over every target's whole window but reads only the
//! results that arrived since the pass before; the rest it still holds.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use futures::stream::{self, StreamExt};
use tokio::sync::mpsc;
use tokio::time::{MissedTickBehavior, interval};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::domain::{CheckOutcome, CheckStatus, NotificationReason, OrgId, Recovered, Target};
use crate::error::Result;
use crate::escalation::IncidentSignal;
use crate::storage::RecentOutcomes;
use crate::storage::admin::{EnabledTargetStream, PublicTargetCursor};

use recent::{ByRegion, Read, Recent};

mod decide;
mod memory;
mod pg;
mod recent;
#[cfg(test)]
mod tests;

pub use decide::{Action, Hold, decide, decide_multi};
pub use memory::{InMemoryIncidentStore, MemIncident};
pub use pg::PgIncidentStore;

/// Persistence handle for the `incidents` table — abstracted so the writer
/// can be unit-tested without a live database. Every method takes `org` so a
/// single store instance can service every tenant.
#[async_trait]
pub trait IncidentStore: Send + Sync {
    async fn open_for_target(&self, org: OrgId, target_id: Uuid) -> Result<Option<OpenIncident>>;
    /// Batched cross-tenant lookup: one SQL round-trip resolves the open
    /// `OpenIncident` for every `(org, target)` pair in the page. The list is
    /// 0-or-1 under the unique open-incident index; it stays a `Vec` so a future
    /// region-scoped grain needs no signature change.
    async fn open_for_pairs(
        &self,
        pairs: &[(OrgId, Uuid)],
    ) -> Result<HashMap<(OrgId, Uuid), Vec<OpenIncident>>>;
    /// When each pair's latest monitor-origin incident ended. Results at or
    /// before that instant were already judged, or were what a human resolved,
    /// so only newer ones may open the next incident. Pairs with no closed
    /// incident are absent.
    async fn last_closed_for_pairs(
        &self,
        pairs: &[(OrgId, Uuid)],
    ) -> Result<HashMap<(OrgId, Uuid), DateTime<Utc>>>;
    /// `None` = no row written: a concurrent writer already holds the open
    /// incident for this target (the DB unique index won the race), or an
    /// incident for it closed at or after `started_at`, so the evidence
    /// predates a resolution. The caller must not page.
    async fn insert_open(&self, org: OrgId, new: NewOpenIncident) -> Result<Option<Uuid>>;
    /// `Some` = this call flipped the incident to resolved; `None` = it was
    /// already closed (lost the race), so the caller must not page.
    async fn close(
        &self,
        org: OrgId,
        incident_id: Uuid,
        ended_at: DateTime<Utc>,
    ) -> Result<Option<Closed>>;
    /// Move `regions` from not-confirmed to confirmed on a still-open incident.
    /// A union, so two writers widening at once both land.
    async fn widen(&self, org: OrgId, incident_id: Uuid, regions: &[String]) -> Result<()>;
    /// Mark a still-open incident down, with the cause of the failure that
    /// took it there when there is one, and note it on the timeline. Never
    /// lowers it, so two writers escalating at once leave one note.
    async fn escalate(
        &self,
        org: OrgId,
        incident_id: Uuid,
        error_sample: Option<String>,
    ) -> Result<()>;
    /// Mark a still-open incident recovering since `since`, or no longer
    /// recovering with `None`.
    async fn set_recovering(
        &self,
        org: OrgId,
        incident_id: Uuid,
        since: Option<DateTime<Utc>>,
    ) -> Result<()>;
    /// Keep a stretch a still-open incident was back up in, once however often
    /// it is sent, and drop the recovering mark that stretch began.
    async fn relapse(&self, org: OrgId, incident_id: Uuid, recovered: Recovered) -> Result<()>;
}

/// A close that landed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Closed {
    /// The group incident this member's close closed with it, its last member
    /// down.
    pub group_incident: Option<Uuid>,
}

#[derive(Debug, Clone)]
pub struct OpenIncident {
    pub id: Uuid,
    pub target_id: Uuid,
    pub started_at: DateTime<Utc>,
    /// `None` = whole-target incident; `Some(r)` = scoped to one region.
    pub region: Option<String>,
    /// Regions that have confirmed the failure so far.
    pub regions_down: Vec<String>,
    /// The worst status the outage has reached, kept in `status_at_start`.
    pub worst_status: CheckStatus,
    /// When the recovery it is holding began; `None` while it is down.
    pub recovering_since: Option<DateTime<Utc>>,
    /// Stretches already kept as back up, so a scan does not keep one twice.
    pub recovered: Vec<Recovered>,
}

#[derive(Debug, Clone)]
pub struct NewOpenIncident {
    pub target_id: Uuid,
    pub started_at: DateTime<Utc>,
    pub status_at_start: CheckStatus,
    pub check_count: u32,
    pub error_sample: Option<String>,
    pub region: Option<String>,
    /// Regions that had confirmed the failure at open, and those that had not
    /// yet (empty for a single-region monitor). A late region moves across
    /// once it confirms; nothing moves back.
    pub regions_down: Vec<String>,
    pub regions_up: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct IncidentWriterConfig {
    /// Scan cadence. Must stay below `lookback` or consecutive scans leave a
    /// blind gap; set at the fastest check interval so detection isn't tick-bound.
    pub tick_interval: Duration,
    /// Floor for the per-target lookback window (it grows with each target's
    /// interval; see `lookback_for`) so fast monitors still carry enough history.
    pub lookback: ChronoDuration,
    /// Minimum consecutive checks needed to confirm a transition. Default 2
    /// absorbs single-result flaps.
    pub flap_threshold: u32,
    /// Max results held per component and region. A safety cap so a hot
    /// loop of high-frequency checks doesn't blow up memory.
    pub max_results_per_tick: usize,
    /// How often a target's whole window is read again rather than only what
    /// arrived. Bounds how long a row a pass missed, or one since deleted,
    /// can keep the held window apart from the stored one.
    pub full_reload_every: Duration,
    /// Number of `(org, target)` rows loaded per database round-trip during
    /// the cross-tenant walk. Bounds what the walk itself holds to roughly
    /// `page_size * sizeof(Target)`; the results in each target's window are
    /// held for every enabled target.
    pub page_size: usize,
    /// Max concurrent `process_target` futures per page. Keeps peak DB
    /// connection demand bounded so the writer can't starve foreground
    /// traffic. Tune below the Postgres pool size.
    pub max_concurrency: usize,
}

impl Default for IncidentWriterConfig {
    fn default() -> Self {
        Self {
            tick_interval: Duration::from_secs(30),
            lookback: ChronoDuration::minutes(10),
            flap_threshold: 2,
            max_results_per_tick: 1_000,
            full_reload_every: Duration::from_secs(3600),
            page_size: 256,
            max_concurrency: 4,
        }
    }
}

pub struct IncidentWriter {
    /// Cross-org stream of every enabled target. The writer never holds a
    /// single org — [`EnabledTargetStream`] is the only surface it talks to for
    /// discovery, and it is keyset-paginated so the per-tick memory and
    /// database load stay bounded as the tenant count grows.
    targets: Arc<dyn EnabledTargetStream>,
    results_store: Arc<dyn RecentOutcomes>,
    incident_store: Arc<dyn IncidentStore>,
    recent: Recent,
    cfg: IncidentWriterConfig,
    /// Notifies the escalation engine when an incident opens or auto-resolves.
    /// `None` leaves the writer paging-agnostic (tests).
    signal_tx: Option<mpsc::Sender<IncidentSignal>>,
}

impl IncidentWriter {
    pub fn new(
        targets: Arc<dyn EnabledTargetStream>,
        results_store: Arc<dyn RecentOutcomes>,
        incident_store: Arc<dyn IncidentStore>,
        cfg: IncidentWriterConfig,
    ) -> Self {
        debug_assert!(
            cfg.lookback
                > ChronoDuration::from_std(cfg.tick_interval).unwrap_or(ChronoDuration::MAX),
            "lookback must exceed tick_interval or scans leave a blind gap"
        );
        Self {
            targets,
            results_store,
            incident_store,
            recent: Recent::default(),
            cfg,
            signal_tx: None,
        }
    }

    /// Wire the escalation-engine signal channel so opens/resolves page.
    pub fn with_signals(mut self, tx: mpsc::Sender<IncidentSignal>) -> Self {
        self.signal_tx = Some(tx);
        self
    }

    fn signal(&self, org: OrgId, incident_id: Uuid, reason: NotificationReason) {
        if let Some(tx) = &self.signal_tx {
            // Non-blocking: incident detection must never stall behind paging
            // throughput. A full channel under a mass outage drops the nudge
            // (logged); the row records the page it owes, which the
            // engine's sweep sends a tick later.
            if let Err(err) = tx.try_send(IncidentSignal {
                org,
                incident_id,
                reason,
            }) {
                metrics::counter!(
                    crate::metric_names::ALERTS_DROPPED,
                    "reason" => reason.as_db_str()
                )
                .increment(1);
                tracing::warn!(%org, %incident_id, error = %err, "incident paging signal dropped");
            }
        }
    }

    /// Drives the writer until `shutdown` fires. Errors during a tick are
    /// logged but never abort the loop — the next tick retries from scratch.
    pub async fn run(&self, shutdown: CancellationToken) {
        let mut ticker = interval(self.cfg.tick_interval);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
        // First tick fires immediately; useful so unit tests don't have to
        // sleep through the interval to observe one cycle.
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => {
                    tracing::debug!("incident_writer: shutdown received");
                    return;
                }
                _ = ticker.tick() => {
                    if let Err(err) = self.tick_once().await {
                        tracing::warn!(error = %err, "incident_writer tick failed");
                    }
                }
            }
        }
    }

    /// One iteration over every enabled target in every live org. Visible
    /// for tests so they can drive a deterministic single tick without
    /// sleeping.
    ///
    /// Walks the cross-tenant target set with keyset pagination so peak RAM
    /// of the walk is `O(page_size)` regardless of org count. Per page the open
    /// incidents and recent results are both resolved in batched reads (one
    /// round-trip for opens, one per lookback tier for results, not one per
    /// target), then per-target work runs concurrently up to
    /// `max_concurrency`. A per-target error logs and continues so one
    /// tenant's failure can't stall the rest.
    pub async fn tick_once(&self) -> Result<()> {
        let now = Utc::now();
        let concurrency = self.cfg.max_concurrency.max(1);

        let mut cursor: Option<PublicTargetCursor> = None;
        loop {
            let page = self
                .targets
                .next_enabled_target_page(cursor, self.cfg.page_size)
                .await?;
            let Some(last) = page.last() else {
                self.recent.forget_idle(self.cfg.full_reload_every);
                return Ok(());
            };
            cursor = Some(PublicTargetCursor::after(last.0, last.1.id));

            let pairs: Vec<(OrgId, Uuid)> = page.iter().map(|(o, t)| (*o, t.id)).collect();
            let (open_map, closed_map, results) = tokio::try_join!(
                self.incident_store.open_for_pairs(&pairs),
                self.incident_store.last_closed_for_pairs(&pairs),
                self.load_page_results(&page, now),
            )?;
            let (open_map, closed_map) = (Arc::new(open_map), Arc::new(closed_map));
            let mut results = results;

            stream::iter(page.into_iter().map(|(org, target)| {
                let open_map = open_map.clone();
                let closed_map = closed_map.clone();
                let window = results.remove(&target.id).unwrap_or_default();
                async move {
                    let open = open_map.get(&(org, target.id)).cloned().unwrap_or_default();
                    let last_closed = closed_map.get(&(org, target.id)).copied();
                    if let Err(err) = self
                        .process_target(org, &target, open, last_closed, window, now)
                        .await
                    {
                        tracing::warn!(
                            %org,
                            target_id = %target.id,
                            error = %err,
                            "incident_writer per-target failed"
                        );
                    }
                }
            }))
            .buffer_unordered(concurrency)
            .for_each(|_| async {})
            .await;
        }
    }

    /// One read per lookback tier instead of one per target, so a slow monitor
    /// never widens a fast one's scan window. A target read before is asked
    /// only for what arrived since, by the store's clock rather than the
    /// results' own, and its window is filled from what is held.
    async fn load_page_results(
        &self,
        page: &[(OrgId, Target)],
        now: DateTime<Utc>,
    ) -> Result<HashMap<Uuid, ByRegion>> {
        let mut reads: BTreeMap<(i64, bool), Pending> = BTreeMap::new();
        for (org, target) in page {
            let lookback = self.lookback_for(target);
            let last_read = self
                .recent
                .last_read(target.id, lookback, self.cfg.full_reload_every);
            let pending = reads
                .entry((tier_seconds(lookback), last_read.is_none()))
                .or_default();
            pending.pairs.push((*org, target.id));
            pending.lookbacks.push((target.id, lookback));
            pending.oldest_read = match (pending.oldest_read, last_read) {
                (Some(oldest), Some(read)) => Some(oldest.min(read)),
                (oldest, read) => oldest.or(read),
            };
        }

        for ((tier_secs, whole), pending) in reads {
            let read = Read {
                whole,
                at: Instant::now(),
                now,
            };
            let arrived_within = pending
                .oldest_read
                .map(|at| read.at.duration_since(at) + ARRIVAL_OVERLAP);
            let rows = self
                .results_store
                .recent_outcomes(
                    &pending.pairs,
                    now - ChronoDuration::seconds(tier_secs),
                    arrived_within,
                    self.cfg.max_results_per_tick,
                )
                .await?;
            self.recent.absorb(
                &pending.lookbacks,
                rows,
                read,
                self.cfg.max_results_per_tick,
            );
        }
        Ok(self.recent.windows(page.iter().map(|(_, t)| t.id)))
    }

    /// Window sized to the target's cadence: confirming a transition needs
    /// `confirmations` results spaced one `interval` apart, which a fixed window
    /// can't hold for a slow monitor, and closing needs the start of a recovery
    /// that has held for the recovery period. Both get the same 2× slack.
    /// `cfg.lookback` floors it for fast ones.
    fn lookback_for(&self, target: &Target) -> ChronoDuration {
        let confirmations = u64::from(target.alert_confirmations.max(1));
        let recovery = u64::from(target.recovery_period_secs);
        let needed = ChronoDuration::seconds(
            (2 * (target.interval.as_secs() * confirmations + recovery)) as i64,
        );
        self.cfg.lookback.max(needed)
    }

    async fn process_target(
        &self,
        org: OrgId,
        target: &Target,
        open: Vec<OpenIncident>,
        last_closed: Option<DateTime<Utc>>,
        window: ByRegion,
        now: DateTime<Utc>,
    ) -> Result<()> {
        // Each region evaluated on its own ASC run, never interleaved. A result
        // a skewed agent clock stamped ahead of now has not happened yet. A
        // result from before the last incident closed was either judged
        // already or resolved by hand; counting it again would reopen the
        // same outage.
        let by_region: Vec<(String, Vec<CheckOutcome>)> = window
            .into_iter()
            .filter_map(|(region, mut outcomes)| {
                outcomes.retain(|r| {
                    r.timestamp < now && last_closed.is_none_or(|closed| r.timestamp > closed)
                });
                (!outcomes.is_empty()).then_some((region, outcomes))
            })
            .collect();

        let confirmations = target.alert_confirmations.max(1);
        let quorum = target.region_policy.required(by_region.len());
        let hold = Hold {
            period: ChronoDuration::seconds(i64::from(target.recovery_period_secs)),
            interval: ChronoDuration::from_std(target.interval).unwrap_or(ChronoDuration::zero()),
            now,
        };
        let actions = decide_multi(target.id, &open, &by_region, confirmations, quorum, hold);
        for action in actions {
            match action {
                Action::None => {}
                Action::Open(new) => {
                    if let Some(id) = self.incident_store.insert_open(org, new).await? {
                        tracing::info!(%org, target_id = %target.id, incident_id = %id, "incident opened");
                        self.signal(org, id, NotificationReason::Opened);
                    }
                }
                Action::Close {
                    incident_id,
                    ended_at,
                } => {
                    if let Some(closed) = self
                        .incident_store
                        .close(org, incident_id, ended_at)
                        .await?
                    {
                        tracing::info!(%org, target_id = %target.id, incident_id = %incident_id, "incident closed");
                        self.signal(org, incident_id, NotificationReason::Resolved);
                        if let Some(group_incident) = closed.group_incident {
                            self.signal(org, group_incident, NotificationReason::Resolved);
                        }
                    }
                }
                Action::Widen {
                    incident_id,
                    regions,
                } => {
                    self.incident_store
                        .widen(org, incident_id, &regions)
                        .await?;
                }
                Action::Escalate {
                    incident_id,
                    error_sample,
                } => {
                    self.incident_store
                        .escalate(org, incident_id, error_sample)
                        .await?;
                }
                Action::Recovering { incident_id, since } => {
                    self.incident_store
                        .set_recovering(org, incident_id, since)
                        .await?;
                }
                Action::Relapsed {
                    incident_id,
                    recovered,
                } => {
                    self.incident_store
                        .relapse(org, incident_id, recovered)
                        .await?;
                }
            }
        }
        Ok(())
    }
}

/// How far back of a target's last read the next one reaches. A row is
/// stamped with its arrival slightly before a read can see it (an async insert
/// waits out its flush window, and the read itself may queue), and a row read
/// twice costs nothing.
const ARRIVAL_OVERLAP: Duration = Duration::from_secs(300);

/// The targets of one read, each with its own lookback, and when the stalest
/// of them was last read.
#[derive(Default)]
struct Pending {
    pairs: Vec<(OrgId, Uuid)>,
    lookbacks: Vec<(Uuid, ChronoDuration)>,
    oldest_read: Option<Instant>,
}

/// Round a per-target lookback window up to a coarse tier so similar-cadence
/// targets share one batched read. Past the ladder the exact window is used
/// (rare, very slow monitors).
fn tier_seconds(window: ChronoDuration) -> i64 {
    const LADDER: [i64; 6] = [
        15 * 60,
        60 * 60,
        6 * 60 * 60,
        24 * 60 * 60,
        7 * 24 * 60 * 60,
        30 * 24 * 60 * 60,
    ];
    let secs = window.num_seconds().max(1);
    LADDER.into_iter().find(|&t| secs <= t).unwrap_or(secs)
}

impl PartialEq for NewOpenIncident {
    fn eq(&self, other: &Self) -> bool {
        self.target_id == other.target_id
            && self.started_at == other.started_at
            && self.status_at_start == other.status_at_start
            && self.check_count == other.check_count
            && self.error_sample == other.error_sample
            && self.region == other.region
            && self.regions_down == other.regions_down
            && self.regions_up == other.regions_up
    }
}
