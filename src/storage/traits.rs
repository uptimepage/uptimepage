use std::collections::HashMap;

use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use uuid::Uuid;

use crate::api::types::{
    AvailabilityBucket, DashboardMetrics, DashboardSparkBucket, FleetRibbonBucket, LatencyBucket,
    PriorPeriodSummary, RegionLatencySeries, RegionRollup, TagCount, TargetsSummary,
};
use crate::domain::agent_wire::FlowRunRecord;
use crate::domain::{
    CheckResult, CheckStatus, HeartbeatPingRecord, NewTarget, NewTargetWithRegions,
    ObservedCadence, OrgId, Target, TargetUpdate, UserId, WriteSource,
};
use crate::error::Result;

/// Which targets a bulk tag add actually touched, and which were already full.
#[derive(Debug, Default)]
pub struct TagAddOutcome {
    pub updated: Vec<Uuid>,
    pub over_cap: Vec<Uuid>,
}

#[async_trait]
pub trait ResultSink: Send + Sync {
    async fn write_batch(&self, results: &[CheckResult]) -> Result<()>;

    /// Write results tagged with the producing region + agent. The default
    /// ignores the tags (home path); CH overrides to stamp them. Used by the
    /// agent ingest API to attribute results to the submitting region.
    async fn write_batch_tagged(
        &self,
        results: &[CheckResult],
        _region: &str,
        _agent_id: &str,
    ) -> Result<()> {
        self.write_batch(results).await
    }
}

/// Where a flow run's trace and page snapshot go. Infallible by signature: a
/// run is telemetry about a check, never its verdict, which reaches storage by
/// its own path. Implementations log and drop rather than hand back a failure a
/// caller must not act on.
#[async_trait]
pub trait FlowRunSink: Send + Sync {
    async fn write_runs(&self, runs: &[FlowRunRecord]);

    /// Write runs attributed to the producing region, mirroring
    /// [`ResultSink::write_batch_tagged`]. The default ignores the tag (home
    /// path, where the sink already knows its own region).
    async fn write_runs_tagged(&self, runs: &[FlowRunRecord], _region: &str) {
        self.write_runs(runs).await;
    }
}

/// Infallible by signature for the same reason as [`FlowRunSink`]: the ping
/// already moved the state the verdict reads, so a failed write costs history,
/// never correctness.
#[async_trait]
pub trait HeartbeatPingSink: Send + Sync {
    async fn write_ping(&self, ping: &HeartbeatPingRecord);
}

/// One stored flow run. `evidence` is `None` both once its shorter window has
/// passed and when the run never captured a page — `evidence_expired` is what
/// tells the two apart, decided by the table that applied the window.
#[derive(Debug, Clone)]
pub struct FlowRunView {
    pub timestamp: DateTime<Utc>,
    pub region: String,
    pub status: CheckStatus,
    pub duration_ms: u32,
    pub stopped_step: Option<usize>,
    pub error: Option<String>,
    pub steps: Vec<crate::domain::agent_wire::StepTrace>,
    pub evidence: Option<crate::domain::agent_wire::FlowEvidence>,
    pub evidence_expired: bool,
}

#[derive(Debug, Default, Clone)]
pub struct TargetFilter {
    pub limit: Option<usize>,
    pub offset: usize,
    /// ILIKE substring against `name`. Callers pass `None` (not `Some("")`)
    /// for "no filter" — the store does not normalise empty strings.
    pub q: Option<String>,
    pub tag: Option<String>,
    pub enabled: Option<bool>,
    pub group: Option<String>,
    pub owner: Option<Uuid>,
    /// `check_spec` type tag (`http`/`tcp`/`dns`/`tls_cert`/`domain_expiry`).
    pub kind: Option<String>,
    /// Restrict to targets that run in this region (via `target_regions`).
    pub region: Option<String>,
    #[allow(dead_code)]
    pub sort: TargetSort,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum TargetSort {
    #[default]
    RecentActivity,
    Name,
    Created,
}

/// Operator-facing target repository. Every method is scoped to a single
/// `org`: the caller resolves it from the request (`CurrentOrg`) and the
/// implementation refuses to touch any other tenant's rows. The `org`
/// parameter is non-optional precisely so "forgot to scope" is a compile
/// error, not a cross-tenant data leak. Scheduler-wide (cross-org)
/// enumeration is a different trait — see `storage::admin::AdminRepo`.
#[async_trait]
pub trait TargetStore: Send + Sync {
    async fn list(&self, org: OrgId, filter: TargetFilter) -> Result<Vec<Target>>;
    async fn get(&self, org: OrgId, id: Uuid) -> Result<Option<Target>>;
    /// `check kind → count` across the whole org for the given filter, with the
    /// filter's own `kind` ignored. Powers the type-chip tallies so they stay
    /// org-wide and invariant when switching chips (not page-scoped).
    async fn count_by_kind(
        &self,
        org: OrgId,
        filter: TargetFilter,
    ) -> Result<std::collections::HashMap<String, i64>>;
    /// Distinct non-null `group_name`s across the org, sorted. Powers the
    /// group filter dropdown so its options are org-wide, not page-scoped.
    async fn distinct_groups(&self, org: OrgId) -> Result<Vec<String>>;
    /// `id → name` for every live target in the org. A projection: it skips the
    /// `check_spec` decode + secret decrypt that `list` pays, for callers that
    /// only need labels (incident console, reports).
    async fn names(&self, org: OrgId) -> Result<std::collections::HashMap<Uuid, String>>;
    /// `id → (name, check kind)` for every live target in the org. Like
    /// [`names`](Self::names) but also returns the check discriminator
    /// (`http`/`tcp`/`dns`/`tls_cert`/`domain_expiry`), read straight from the
    /// `check_spec` tag — no full spec decode or secret decrypt.
    async fn names_and_kinds(
        &self,
        org: OrgId,
    ) -> Result<std::collections::HashMap<Uuid, (String, String)>>;
    /// `(kind, host)` for the org's targets of the given kinds, read off the
    /// `check_spec` tag with no full decode. The kind filter keeps this to the
    /// handful of expiry-shaped monitors an org has, not its whole inventory.
    async fn hosts_by_kind(&self, org: OrgId, kinds: &[&str]) -> Result<Vec<(String, String)>>;
    /// Create one target. `max_targets` is the plan cap; the INSERT is
    /// guarded by `(count) + 1 <= max_targets` so the bound holds even
    /// against a concurrent create (no check-then-act). A flow monitor is
    /// additionally held to `max_flow_checks` under the same lock. Returns
    /// `AppError::QuotaExceeded` when either cap is reached.
    async fn create(
        &self,
        org: OrgId,
        new: NewTarget,
        source: WriteSource,
        max_targets: i64,
        max_flow_checks: i64,
    ) -> Result<Target>;
    /// A `source` of `None` leaves `write_source` as it was, for a writer that
    /// must not claim authorship and erase another tool's marker. An update that
    /// flips `enabled` writes a `target.paused`/`target.resumed` audit row in the
    /// same transaction, since a paused monitor simply stops reporting and
    /// nothing else records who stopped it.
    async fn update(
        &self,
        org: OrgId,
        id: Uuid,
        update: TargetUpdate,
        source: Option<WriteSource>,
        actor: Option<UserId>,
    ) -> Result<Option<Target>>;
    /// A monitor is a hard delete, so the `target.deleted` audit row written in
    /// the same transaction is the only surviving record that it existed.
    async fn delete(&self, org: OrgId, id: Uuid, actor: Option<UserId>) -> Result<bool>;
    /// Remove every alert binding to `channel_id` across the org's targets.
    /// Channel deletion calls this so a dangling binding can't poison later
    /// whole-array alert updates. Returns the number of targets touched.
    async fn unbind_channel(&self, org: OrgId, channel_id: Uuid) -> Result<u64>;
    /// Bulk create. Same atomic `(count) + items.len() <= max_targets`
    /// bound; rows, their region sets and policies land together or not at all.
    async fn bulk_create(
        &self,
        org: OrgId,
        items: Vec<NewTargetWithRegions>,
        source: WriteSource,
        max_targets: i64,
        max_flow_checks: i64,
    ) -> Result<Vec<Target>>;
    async fn list_updated_since(&self, org: OrgId, since: DateTime<Utc>) -> Result<Vec<Target>>;
    /// Aggregate tag inventory across all targets. `prefix` filters tag names
    /// for autocomplete; `limit` caps the number of returned rows. Sorted by
    /// descending count, then alphabetical.
    async fn list_tags(
        &self,
        org: OrgId,
        prefix: Option<String>,
        limit: usize,
    ) -> Result<Vec<TagCount>>;
    /// Totals + enabled/disabled split for the dashboard.
    async fn summary(&self, org: OrgId) -> Result<TargetsSummary>;
    /// Atomically enable or disable each id; returns the set that existed. Ids
    /// whose state actually changed are named in one `target.paused`/
    /// `target.resumed` audit row written in the same transaction.
    async fn set_enabled(
        &self,
        org: OrgId,
        ids: &[Uuid],
        enabled: bool,
        actor: Option<UserId>,
    ) -> Result<Vec<Uuid>>;
    /// Atomically delete each id; returns the set that existed. One
    /// `target.bulk_deleted` audit row, not one per id: a 10 000-id call must
    /// not flood the log it is meant to be recorded in.
    async fn delete_bulk(
        &self,
        org: OrgId,
        ids: &[Uuid],
        actor: Option<UserId>,
    ) -> Result<Vec<Uuid>>;
    /// Adds `tags` to every named target. One whose merged list would go over
    /// the tag cap is left alone and reported in `over_cap`, so the caller can
    /// say why instead of calling it missing.
    async fn add_tags(&self, org: OrgId, ids: &[Uuid], tags: &[String]) -> Result<TagAddOutcome>;
    /// Removes `tags` from every named target; returns the set that existed.
    async fn remove_tags(&self, org: OrgId, ids: &[Uuid], tags: &[String]) -> Result<Vec<Uuid>>;
    /// Sets every named target's `group_name` to `group` (None clears).
    /// Returns the set that existed.
    async fn set_group(&self, org: OrgId, ids: &[Uuid], group: Option<&str>) -> Result<Vec<Uuid>>;
    /// Liveness probe for `/readyz` — connection-level, not tenant data.
    async fn ping(&self) -> Result<()>;
    /// Distinct regions this org's targets are assigned to, sorted, for the
    /// dashboard region selector. Empty or single-element means a single-region
    /// org. Reads `target_regions` (config), not check history.
    async fn regions_for_org(&self, org: OrgId) -> Result<Vec<String>>;
    /// Region catalog: ids of every enabled region, sorted. The set a monitor
    /// may be assigned to. Global (regions are operator-defined), so no `org`.
    async fn available_regions(&self) -> Result<Vec<String>>;
    /// Same catalog with display fields for the assignment picker, so the UI
    /// can show a human name + location instead of the bare id.
    async fn available_regions_detailed(&self) -> Result<Vec<RegionOption>>;
    /// Enabled regions a new monitor starts assigned to, sorted. A subset of
    /// [`Self::available_regions`]: the rest stay pickable, just unchecked, so a
    /// region with a known-bad network path is opt-in rather than default.
    async fn default_selected_regions(&self) -> Result<Vec<String>>;
    /// Regions one target is assigned to, sorted. `None` if the target is not in
    /// the org (so a guessed id from another tenant reads as not-found).
    async fn regions_for_target(&self, org: OrgId, target_id: Uuid) -> Result<Option<Vec<String>>>;
    /// Enabled regions with at least one enabled, flow-capable agent. A flow
    /// monitor must land on one of these or it never runs.
    async fn flow_capable_regions(&self) -> Result<Vec<String>>;
    /// Replace a target's region assignments with `regions`. `false` if the
    /// target is not in the org. Caller validates the regions exist + are
    /// enabled and within the plan's `max_regions`.
    async fn set_target_regions(
        &self,
        org: OrgId,
        target_id: Uuid,
        regions: &[String],
    ) -> Result<bool>;
}

/// A region for the monitor assignment picker: id plus display fields.
/// `name`/`city` are empty when unset; the UI shows `name` (falling back to
/// `id`) and appends `city` when present.
#[derive(Debug, Clone)]
pub struct RegionOption {
    pub id: String,
    pub name: String,
    pub city: String,
    /// ISO 3166-1 alpha-2 country, when set — drives the flag emoji.
    pub country_code: Option<String>,
    /// Continent slug (see `domain::region::Continent`) for picker grouping.
    pub continent: Option<String>,
    /// Coordinates for a future map; both set or both `None`.
    pub latitude: Option<f64>,
    pub longitude: Option<f64>,
}

impl RegionOption {
    /// Catalog display name: `name`, falling back to the id when unset or
    /// auto-seeded equal to the id (the control-plane region).
    pub fn display_name(&self) -> &str {
        let name = self.name.trim();
        if name.is_empty() || name == self.id {
            &self.id
        } else {
            name
        }
    }

    /// Unicode flag emoji for `country_code`, or `None` when unset/invalid.
    pub fn flag(&self) -> Option<String> {
        crate::domain::region::country_flag(self.country_code.as_deref()?)
    }
}

#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
pub struct TimeRange {
    pub from: DateTime<Utc>,
    pub to: DateTime<Utc>,
}

/// A [`TimeRange`] that passed a retention decision. The tenant read methods
/// take this, not a bare `TimeRange`, so the compiler refuses any read that
/// didn't pick a window — closing the per-surface bypass class.
#[derive(Debug, Clone, Copy)]
pub struct ClampedRange(TimeRange);

impl ClampedRange {
    /// Clamp `from` forward to `window_days` before `now`. `window_days` comes
    /// from the plan, keeping storage free of the plan model.
    pub fn for_window(range: TimeRange, window_days: i64, now: DateTime<Utc>) -> Self {
        let floor = now - Duration::try_days(window_days.max(0)).unwrap_or_default();
        Self(TimeRange {
            from: range.from.max(floor),
            to: range.to,
        })
    }

    /// System read that must see the full window (incident detection, building
    /// the public view) — explicit opt-out from the clamp.
    pub fn unclamped(range: TimeRange) -> Self {
        Self(range)
    }

    pub fn inner(&self) -> TimeRange {
        self.0
    }
}

impl std::ops::Deref for ClampedRange {
    type Target = TimeRange;
    fn deref(&self) -> &TimeRange {
        &self.0
    }
}

/// Round a requested bucket width up to a whole number of 60s rollup rows, so
/// every output bucket spans an integer count of minutes (grid alignment is the
/// caller's `toStartOfInterval` / `div_euclid`).
pub fn rollup_bucket_secs(bucket_seconds: u32) -> u32 {
    bucket_seconds.max(60).div_ceil(60) * 60
}

#[derive(Debug, Default, Clone, Copy, serde::Serialize, utoipa::ToSchema)]
pub struct UptimeStats {
    pub total: u64,
    pub up: u64,
    pub down: u64,
    pub degraded: u64,
    pub error: u64,
    /// `None` for an empty window: no observations is not 0% uptime.
    #[schema(example = 99.94, nullable)]
    pub uptime_pct: Option<f64>,
}

impl UptimeStats {
    pub fn from_results(results: &[CheckResult]) -> Self {
        let mut stats = Self::default();
        for r in results {
            stats.total += 1;
            match r.status {
                CheckStatus::Up => stats.up += 1,
                CheckStatus::Down => stats.down += 1,
                CheckStatus::Degraded => stats.degraded += 1,
                CheckStatus::Error => stats.error += 1,
            }
        }
        stats.finish();
        stats
    }

    /// Shared by both stores so their uptime can't drift.
    pub(crate) fn finish(&mut self) {
        self.uptime_pct = (self.total > 0).then(|| (self.up as f64 / self.total as f64) * 100.0);
    }
}

/// How unsettled one region was over a window. Failures that never confirm an
/// incident leave no other trace, so this is the only thing that separates a
/// flapping probe path from a quiet one.
#[derive(Debug, Clone, Default)]
pub struct RegionFlaps {
    pub region: String,
    pub failures: u64,
    pub transitions: u64,
}

#[derive(Debug, Clone)]
pub struct RegionLatestStatus {
    pub target_id: Uuid,
    pub region: String,
    pub status: CheckStatus,
}

impl RegionLatestStatus {
    pub fn group(rows: Vec<Self>) -> HashMap<Uuid, Vec<CheckStatus>> {
        let mut out: HashMap<Uuid, Vec<CheckStatus>> = HashMap::new();
        for r in rows {
            out.entry(r.target_id).or_default().push(r.status);
        }
        out
    }
}

/// Operator-facing results repository. Org-scoped on every method for the
/// same reason as [`TargetStore`]: the `org` is resolved from the request and
/// the implementation never returns another tenant's check history. A bare
/// `target_id` is not enough — a UUID guessed from another org must resolve to
/// zero rows, not that org's data.
#[async_trait]
pub trait ResultsStore: Send + Sync {
    /// Liveness probe for `/readyz` — connection-level, not tenant data.
    async fn ping(&self) -> Result<()>;
    /// Newest first, within the range: up to `limit` most recent runs plus up
    /// to `limit` most recent failures, merged — at the interval floor a page of
    /// newest-only reaches back hours while the table holds weeks, so a failure
    /// would otherwise be unreachable. Defaults to empty so a store without the
    /// table simply has no history rather than every fixture reimplementing it.
    async fn flow_runs(
        &self,
        _org: OrgId,
        _target_id: Uuid,
        _range: ClampedRange,
        _region: Option<&str>,
        _limit: usize,
    ) -> Result<Vec<FlowRunView>> {
        Ok(Vec::new())
    }
    /// What the job printed on the failure recorded at `at`. Matched on that
    /// exact instant, never "the newest failure", so a lost log write shows no
    /// output instead of the previous run's.
    async fn heartbeat_failure_output(
        &self,
        _org: OrgId,
        _target_id: Uuid,
        _at: DateTime<Utc>,
    ) -> Result<Option<String>> {
        Ok(None)
    }
    /// Runs the job reported in the window. `/start` is excluded for the same
    /// reason [`Self::heartbeat_cadence`] excludes it, so the two figures on one
    /// page cannot disagree. `None` where the store cannot answer.
    async fn heartbeat_ping_count(
        &self,
        _org: OrgId,
        _target_id: Uuid,
        _range: ClampedRange,
    ) -> Result<Option<u64>> {
        Ok(None)
    }
    /// Gaps between this heartbeat's successes over the last `days`. A ping
    /// landing inside its window writes no check result, so this log is the
    /// only record of how often the job really runs.
    async fn heartbeat_cadence(
        &self,
        _org: OrgId,
        _target_id: Uuid,
        _days: u16,
    ) -> Result<Option<ObservedCadence>> {
        Ok(None)
    }
    /// One series per declared step, bucketed over the range: the mean
    /// duration among the runs that *passed* it, plus how many failed. A
    /// failed step sat in its whole step timeout, so averaging it in lets a
    /// handful of failures bury the timings of every run around them. Reads
    /// the run table directly rather than a rollup — the interval floor caps a
    /// flow at 288 runs a day, small enough to aggregate per request.
    async fn flow_step_buckets(
        &self,
        _org: OrgId,
        _target_id: Uuid,
        _range: ClampedRange,
        _bucket_seconds: u32,
        _region: Option<&str>,
    ) -> Result<Vec<crate::api::types::FlowStepTrend>> {
        Ok(Vec::new())
    }
    async fn list_results(
        &self,
        org: OrgId,
        target_id: Uuid,
        range: ClampedRange,
        limit: usize,
        offset: usize,
        region: Option<&str>,
    ) -> Result<Vec<CheckResult>>;
    /// Recent results for one target, each paired with its region, in one query.
    /// The caller groups by region so per-region runs don't interleave.
    async fn list_results_by_region(
        &self,
        org: OrgId,
        target_id: Uuid,
        range: ClampedRange,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<(String, CheckResult)>>;
    /// Failing results (status is not `Up`) for one target over a window, each
    /// paired with its region, newest first. Backs the ribbon drill drawer,
    /// which only opens on a failing bucket, so success rows would just bury the
    /// failures the user came to see. `region` scopes to one region when set.
    async fn list_failures_by_region(
        &self,
        org: OrgId,
        target_id: Uuid,
        range: ClampedRange,
        limit: usize,
        offset: usize,
        region: Option<&str>,
    ) -> Result<Vec<(String, CheckResult)>>;
    /// Recent results for many targets in one query, each row tagged with its
    /// region (the `CheckResult` carries `target_id` and `org_id`). Returns at
    /// most `per_target_limit` newest rows per `(target, region)`. Rows are
    /// filtered to the caller's `(org, target)` pairs, so a target id resolving
    /// to another org yields nothing.
    async fn recent_results_for_targets(
        &self,
        targets: &[(OrgId, Uuid)],
        range: ClampedRange,
        per_target_limit: usize,
    ) -> Result<Vec<(String, CheckResult)>>;
    async fn uptime(
        &self,
        org: OrgId,
        target_id: Uuid,
        range: ClampedRange,
        region: Option<&str>,
    ) -> Result<UptimeStats>;
    /// Aggregate uptime, response, and incident count across all targets
    /// in `range`. Returns `(checks_total, checks_up, avg_ms,
    /// incident_count)`. `avg_ms` is the true sample-weighted mean —
    /// must come from the same source as `checks_total`/`checks_up` so
    /// the dashboard's Δ-vs-prior comparison doesn't mix raw and
    /// per-target-rounded numbers.
    async fn last_n_summary(
        &self,
        org: OrgId,
        range: TimeRange,
        region: Option<&str>,
    ) -> Result<(u64, u64, u32, u64)>;
    /// Per-monitor rollup for the operator dashboard table. One row per
    /// target with samples/up/p50/p95/last_status in `range`. Targets
    /// with no samples are omitted; the caller joins the result with the
    /// target list and synthesises a zero-sample row when needed.
    async fn dashboard_rollup(
        &self,
        org: OrgId,
        range: TimeRange,
        region: Option<&str>,
    ) -> Result<Vec<DashboardMetrics>>;
    /// Each region's latest verdict, to fold against the monitor's policy. A
    /// region with no samples in `range` is absent, and so out of the quorum
    /// denominator.
    async fn latest_status_by_region(
        &self,
        org: OrgId,
        range: TimeRange,
    ) -> Result<Vec<RegionLatestStatus>>;
    /// Minute-bucketed average duration for the last 60 minutes, every
    /// monitor in the org. Drives the per-row sparkline. Implementations
    /// MUST read from the `check_results_1m` rollup (already aggregated
    /// per minute) so the cost stays cheap even for large orgs.
    async fn dashboard_sparkline(
        &self,
        org: OrgId,
        from: chrono::DateTime<Utc>,
        to: chrono::DateTime<Utc>,
        region: Option<&str>,
    ) -> Result<Vec<DashboardSparkBucket>>;
    /// Bucketed latency for a single monitor: p50/p95/p99 + per-phase means
    /// per `bucket_seconds` slice across `range`. Drives the monitor-detail
    /// latency and breakdown charts. Implementations MUST read from the
    /// `check_results_1m` rollup so a 30d window stays O(buckets) regardless
    /// of sample rate. Buckets with no samples are omitted; the chart leaves
    /// the gap unconnected.
    async fn latency_buckets(
        &self,
        org: OrgId,
        target_id: Uuid,
        range: ClampedRange,
        bucket_seconds: u32,
        region: Option<&str>,
    ) -> Result<Vec<LatencyBucket>>;
    /// Per-target up/total counts per bucket — drives the uptime-card
    /// sparkline. Same rollup source and bucketing as [`latency_buckets`](Self::latency_buckets).
    async fn availability_buckets(
        &self,
        org: OrgId,
        target_id: Uuid,
        range: ClampedRange,
        bucket_seconds: u32,
        region: Option<&str>,
    ) -> Result<Vec<AvailabilityBucket>>;
    /// Per-region rollup for one target over `range` — drives the detail-page
    /// region breakdown table. One row per region; regions with no samples are
    /// omitted. Single-region orgs see one row.
    async fn region_breakdown(
        &self,
        org: OrgId,
        target_id: Uuid,
        range: TimeRange,
    ) -> Result<Vec<RegionRollup>>;
    /// Per-region failure and state-change counts for one target over `range`.
    /// Implementations MUST read raw results: the minute rollup keeps one status
    /// per minute, which erases the transitions this counts.
    async fn flap_counts(
        &self,
        org: OrgId,
        target_id: Uuid,
        range: ClampedRange,
    ) -> Result<Vec<RegionFlaps>>;
    /// Per-region latency buckets for one target — the overlay view, each
    /// region a separate line. Same rollup source + bucketing as
    /// [`latency_buckets`](Self::latency_buckets), split by region.
    async fn latency_buckets_by_region(
        &self,
        org: OrgId,
        target_id: Uuid,
        range: ClampedRange,
        bucket_seconds: u32,
    ) -> Result<Vec<RegionLatencySeries>>;
    /// Fleet-wide uptime ribbon: one bucket per `bucket_seconds` slice
    /// across every monitor in `org`. Implementations MUST read from the
    /// `check_results_1m` rollup so the cost stays O(buckets) regardless
    /// of monitor count. Buckets with no samples are omitted; the caller
    /// fills the window into a fixed-length array.
    async fn fleet_ribbon(
        &self,
        org: OrgId,
        from: chrono::DateTime<Utc>,
        to: chrono::DateTime<Utc>,
        bucket_seconds: u32,
        region: Option<&str>,
    ) -> Result<Vec<FleetRibbonBucket>>;
    /// Fleet totals for the period immediately preceding `range` (same
    /// span, ending at `range.from`). Drives the Δ-vs-prior hints on
    /// each KPI card. Implementations MUST read from the same source as
    /// `last_n_summary` so the dashboard never compares matview-derived
    /// totals against raw totals (ingest lag would produce phantom
    /// deltas). Returns zeros when there is no prior data — the view
    /// layer hides the hint in that case.
    async fn prior_period_summary(
        &self,
        org: OrgId,
        range: TimeRange,
        region: Option<&str>,
    ) -> Result<PriorPeriodSummary>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(y: i32, m: u32, d: u32) -> DateTime<Utc> {
        use chrono::TimeZone;
        Utc.with_ymd_and_hms(y, m, d, 0, 0, 0).unwrap()
    }

    #[test]
    fn empty_window_has_no_uptime_rather_than_zero() {
        let s = UptimeStats::from_results(&[]);
        assert_eq!(s.uptime_pct, None, "no observations is not 0% uptime");
    }

    #[test]
    fn uptime_is_the_rate_over_observed_checks() {
        let mut s = UptimeStats {
            total: 4,
            up: 3,
            down: 1,
            ..Default::default()
        };
        s.finish();
        assert_eq!(s.uptime_pct, Some(75.0));
    }

    #[test]
    fn for_window_narrows_out_of_window_from() {
        let now = at(2026, 5, 1);
        let r = ClampedRange::for_window(
            TimeRange {
                from: now - Duration::try_days(60).unwrap(),
                to: now,
            },
            30,
            now,
        );
        assert_eq!(r.from, now - Duration::try_days(30).unwrap());
        assert_eq!(r.to, now);
    }

    #[test]
    fn for_window_leaves_in_window_from() {
        let now = at(2026, 5, 1);
        let from = now - Duration::try_days(7).unwrap();
        let r = ClampedRange::for_window(TimeRange { from, to: now }, 30, now);
        assert_eq!(r.from, from);
    }

    #[test]
    fn unclamped_preserves_range() {
        let now = at(2026, 5, 1);
        let from = now - Duration::try_days(400).unwrap();
        let r = ClampedRange::unclamped(TimeRange { from, to: now });
        assert_eq!(r.from, from);
    }
}
