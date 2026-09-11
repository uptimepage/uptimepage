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
//!  * `≥ flap_threshold` consecutive `up` results while an open incident exists
//!    → UPDATE `ended_at` to the first such timestamp.
//!
//! Both rules are idempotent: re-running with the same input produces no
//! additional writes. The rules themselves are applied by [`decide_multi`],
//! with no database and no clock; this file is the loop that feeds it and
//! writes what it returns.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use futures::stream::{self, StreamExt};
use tokio::sync::mpsc;
use tokio::time::{MissedTickBehavior, interval};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::domain::{CheckResult, CheckStatus, NotificationReason, OrgId, Target};
use crate::error::Result;
use crate::escalation::IncidentSignal;
use crate::storage::ResultsStore;
use crate::storage::admin::{EnabledTargetStream, PublicTargetCursor};
use crate::storage::traits::{ClampedRange, TimeRange};

mod decide;
mod memory;
mod pg;
#[cfg(test)]
mod tests;

pub use decide::{Action, decide, decide_multi};
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
    ) -> Result<std::collections::HashMap<(OrgId, Uuid), Vec<OpenIncident>>>;
    /// `None` = a concurrent writer already holds the open incident for this
    /// target (the DB unique index won the race); the caller must not page.
    async fn insert_open(&self, org: OrgId, new: NewOpenIncident) -> Result<Option<Uuid>>;
    /// `true` = this call flipped the incident to resolved; `false` = it was
    /// already closed (lost the race), so the caller must not page.
    async fn close(&self, org: OrgId, incident_id: Uuid, ended_at: DateTime<Utc>) -> Result<bool>;
    /// Move `regions` from not-confirmed to confirmed on a still-open incident.
    /// A union, so two writers widening at once both land.
    async fn widen(&self, org: OrgId, incident_id: Uuid, regions: &[String]) -> Result<()>;
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
    /// Max results fetched per component per tick. A safety cap so a hot
    /// loop of high-frequency checks doesn't blow up memory.
    pub max_results_per_tick: usize,
    /// Number of `(org, target)` rows loaded per database round-trip during
    /// the cross-tenant walk. Bounds peak writer-side RAM to roughly
    /// `page_size * sizeof(Target)`; independent of total target count.
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
    results_store: Arc<dyn ResultsStore>,
    incident_store: Arc<dyn IncidentStore>,
    cfg: IncidentWriterConfig,
    /// Notifies the escalation engine when an incident opens or auto-resolves.
    /// `None` leaves the writer paging-agnostic (tests, escalation disabled).
    signal_tx: Option<mpsc::Sender<IncidentSignal>>,
}

impl IncidentWriter {
    pub fn new(
        targets: Arc<dyn EnabledTargetStream>,
        results_store: Arc<dyn ResultsStore>,
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
            // (logged); the row is still in the DB for a later reconcile.
            if let Err(err) = tx.try_send(IncidentSignal {
                org,
                incident_id,
                reason,
            }) {
                metrics::counter!(
                    crate::observability::metrics::names::ALERTS_DROPPED,
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
    /// is `O(page_size)` regardless of org count. Per page the open incidents
    /// and recent results are both resolved in batched reads (one round-trip
    /// for opens, one per lookback tier for results, not one per target), then
    /// per-target work runs concurrently up to `max_concurrency`. A per-target
    /// error logs and continues so one tenant's failure can't stall the rest.
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
                return Ok(());
            };
            cursor = Some(PublicTargetCursor::after(last.0, last.1.id));

            let pairs: Vec<(OrgId, Uuid)> = page.iter().map(|(o, t)| (*o, t.id)).collect();
            let open_map = Arc::new(self.incident_store.open_for_pairs(&pairs).await?);
            let results = Arc::new(self.load_page_results(&page, now).await?);

            stream::iter(page.into_iter().map(|(org, target)| {
                let open_map = open_map.clone();
                let results = results.clone();
                async move {
                    let open = open_map.get(&(org, target.id)).cloned().unwrap_or_default();
                    let tagged = results.get(&target.id).cloned().unwrap_or_default();
                    if let Err(err) = self.process_target(org, &target, open, tagged, now).await {
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

    /// One ClickHouse read per lookback tier instead of one per target, so a
    /// slow monitor never widens a fast one's scan window. Each target is then
    /// trimmed to its own window in [`process_target`](Self::process_target).
    async fn load_page_results(
        &self,
        page: &[(OrgId, Target)],
        now: DateTime<Utc>,
    ) -> Result<std::collections::HashMap<Uuid, Vec<(String, CheckResult)>>> {
        let mut tiers: std::collections::BTreeMap<i64, Vec<(OrgId, Uuid)>> =
            std::collections::BTreeMap::new();
        for (org, target) in page {
            let tier = tier_seconds(self.lookback_for(target));
            tiers.entry(tier).or_default().push((*org, target.id));
        }

        let mut out: std::collections::HashMap<Uuid, Vec<(String, CheckResult)>> =
            std::collections::HashMap::new();
        for (tier_secs, targets) in tiers {
            let range = ClampedRange::unclamped(TimeRange {
                from: now - ChronoDuration::seconds(tier_secs),
                to: now,
            });
            let rows = self
                .results_store
                .recent_results_for_targets(&targets, range, self.cfg.max_results_per_tick)
                .await?;
            for (region, r) in rows {
                out.entry(r.target_id).or_default().push((region, r));
            }
        }
        Ok(out)
    }

    /// Window sized to the target's cadence: confirming a transition needs
    /// `confirmations` results spaced one `interval` apart, which a fixed window
    /// can't hold for a slow monitor. `cfg.lookback` floors it for fast ones.
    fn lookback_for(&self, target: &Target) -> ChronoDuration {
        let confirmations = u64::from(target.alert_confirmations.max(1));
        let needed =
            ChronoDuration::seconds((target.interval.as_secs() * 2 * confirmations) as i64);
        self.cfg.lookback.max(needed)
    }

    async fn process_target(
        &self,
        org: OrgId,
        target: &Target,
        open: Vec<OpenIncident>,
        tagged: Vec<(String, CheckResult)>,
        now: DateTime<Utc>,
    ) -> Result<()> {
        // The tier read can be wider than this target's window; trim to its own.
        let cutoff = now - self.lookback_for(target);
        // Each region evaluated on its own ASC run, never interleaved.
        let mut by_region: std::collections::BTreeMap<String, Vec<CheckResult>> =
            std::collections::BTreeMap::new();
        for (region, r) in tagged {
            if r.timestamp >= cutoff {
                by_region.entry(region).or_default().push(r);
            }
        }
        let mut by_region: Vec<(String, Vec<CheckResult>)> = by_region.into_iter().collect();
        for (_, results) in by_region.iter_mut() {
            results.sort_by_key(|r| r.timestamp);
        }

        let confirmations = target.alert_confirmations.max(1);
        let quorum = target.region_policy.required(by_region.len());
        let actions = decide_multi(target.id, &open, &by_region, confirmations, quorum);
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
                    if self
                        .incident_store
                        .close(org, incident_id, ended_at)
                        .await?
                    {
                        tracing::info!(%org, target_id = %target.id, incident_id = %incident_id, "incident closed");
                        self.signal(org, incident_id, NotificationReason::Resolved);
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
            }
        }
        Ok(())
    }
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
