use async_trait::async_trait;
use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use uuid::Uuid;

use crate::api::types::{
    AvailabilityBucket, DashboardMetrics, DashboardSparkBucket, FleetRibbonBucket, LatencyBucket,
    PriorPeriodSummary, RegionLatencySeries, RegionRollup, TagCount, TargetsSummary,
};
use crate::domain::target::MAX_TAGS_PER_TARGET;
use crate::domain::{
    CheckResult, CheckStatus, NewTarget, NewTargetWithRegions, OrgId, Target, TargetUpdate, UserId,
    WriteSource,
};
use crate::error::Result;
use crate::storage::traits::{
    ClampedRange, RegionFlaps, RegionLatestStatus, RegionOption, ResultSink, ResultsStore,
    TagAddOutcome, TargetFilter, TargetStore, TimeRange, UptimeStats, rollup_bucket_secs,
};

#[derive(Default)]
pub struct InMemorySink {
    results: Mutex<Vec<CheckResult>>,
}

impl InMemorySink {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn snapshot(&self) -> Vec<CheckResult> {
        self.results.lock().clone()
    }

    pub fn len(&self) -> usize {
        self.results.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.results.lock().is_empty()
    }
}

#[async_trait]
impl ResultSink for InMemorySink {
    async fn write_batch(&self, results: &[CheckResult]) -> Result<()> {
        self.results.lock().extend_from_slice(results);
        Ok(())
    }
}

// Single-org test fixture. The `org` parameter exists to satisfy the
// org-scoped trait (and to type-check the call sites that thread
// `CurrentOrg`), but is intentionally not used for filtering: these stores
// only ever hold one tenant's data in tests. Real cross-tenant isolation
// lives in the Postgres/ClickHouse impls and is covered by the two-org
// integration tests.
#[async_trait]
impl ResultsStore for InMemorySink {
    async fn ping(&self) -> Result<()> {
        Ok(())
    }

    async fn list_results(
        &self,
        _org: OrgId,
        target_id: Uuid,
        range: ClampedRange,
        limit: usize,
        offset: usize,
        _region: Option<&str>,
    ) -> Result<Vec<CheckResult>> {
        let guard = self.results.lock();
        let mut out: Vec<CheckResult> = guard
            .iter()
            .filter(|r| {
                r.target_id == target_id && r.timestamp >= range.from && r.timestamp < range.to
            })
            .cloned()
            .collect();
        out.sort_by_key(|r| std::cmp::Reverse(r.timestamp));
        let mut paged: Vec<CheckResult> = out.into_iter().skip(offset).collect();
        if paged.len() > limit {
            paged.truncate(limit);
        }
        Ok(paged)
    }

    async fn list_results_by_region(
        &self,
        org: OrgId,
        target_id: Uuid,
        range: ClampedRange,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<(String, CheckResult)>> {
        // No region dimension in memory; tag every row one synthetic region.
        let rows = self
            .list_results(org, target_id, range, limit, offset, None)
            .await?;
        Ok(rows
            .into_iter()
            .map(|r| ("default".to_string(), r))
            .collect())
    }

    async fn list_failures_by_region(
        &self,
        _org: OrgId,
        target_id: Uuid,
        range: ClampedRange,
        limit: usize,
        offset: usize,
        _region: Option<&str>,
    ) -> Result<Vec<(String, CheckResult)>> {
        // No region dimension in memory; drop success rows, tag one region.
        let guard = self.results.lock();
        let mut out: Vec<CheckResult> = guard
            .iter()
            .filter(|r| {
                r.target_id == target_id
                    && r.status != CheckStatus::Up
                    && r.timestamp >= range.from
                    && r.timestamp < range.to
            })
            .cloned()
            .collect();
        out.sort_by_key(|r| std::cmp::Reverse(r.timestamp));
        Ok(out
            .into_iter()
            .skip(offset)
            .take(limit)
            .map(|r| ("default".to_string(), r))
            .collect())
    }

    async fn recent_results_for_targets(
        &self,
        targets: &[(OrgId, Uuid)],
        range: ClampedRange,
        per_target_limit: usize,
    ) -> Result<Vec<(String, CheckResult)>> {
        let wanted: std::collections::HashSet<Uuid> = targets.iter().map(|(_, t)| *t).collect();
        let guard = self.results.lock();
        let mut by_target: std::collections::HashMap<Uuid, Vec<CheckResult>> =
            std::collections::HashMap::new();
        for r in guard.iter() {
            if wanted.contains(&r.target_id) && r.timestamp >= range.from && r.timestamp < range.to
            {
                by_target.entry(r.target_id).or_default().push(r.clone());
            }
        }
        let mut out = Vec::new();
        for mut results in by_target.into_values() {
            results.sort_by_key(|r| std::cmp::Reverse(r.timestamp));
            results.truncate(per_target_limit);
            out.extend(results.into_iter().map(|r| ("default".to_string(), r)));
        }
        Ok(out)
    }

    async fn uptime(
        &self,
        _org: OrgId,
        target_id: Uuid,
        range: ClampedRange,
        _region: Option<&str>,
    ) -> Result<UptimeStats> {
        let guard = self.results.lock();
        let filtered: Vec<CheckResult> = guard
            .iter()
            .filter(|r| {
                r.target_id == target_id && r.timestamp >= range.from && r.timestamp < range.to
            })
            .cloned()
            .collect();
        Ok(UptimeStats::from_results(&filtered))
    }

    async fn dashboard_rollup(
        &self,
        _org: OrgId,
        range: TimeRange,
        _region: Option<&str>,
    ) -> Result<Vec<DashboardMetrics>> {
        let guard = self.results.lock();
        let mut by_target: std::collections::BTreeMap<Uuid, Vec<&CheckResult>> =
            std::collections::BTreeMap::new();
        for r in guard.iter() {
            if r.timestamp < range.from || r.timestamp >= range.to {
                continue;
            }
            by_target.entry(r.target_id).or_default().push(r);
        }
        let mut out = Vec::with_capacity(by_target.len());
        for (tid, mut rows) in by_target {
            rows.sort_by_key(|r| r.timestamp);
            let samples = rows.len() as u64;
            let up = rows.iter().filter(|r| r.status == CheckStatus::Up).count() as u64;
            let mut durations: Vec<u32> = rows.iter().map(|r| r.duration_ms).collect();
            durations.sort_unstable();
            let p50_ms = percentile(&durations, 0.50);
            let p95_ms = percentile(&durations, 0.95);
            let avg_ms = durations
                .iter()
                .map(|d| *d as u64)
                .sum::<u64>()
                .checked_div(samples)
                .map_or(0, |v| v.min(u32::MAX as u64) as u32);
            let last_status = rows
                .last()
                .map(|r| r.status.as_str().to_string())
                .unwrap_or_default();
            let last_minute_ts = rows.last().map(|r| {
                let secs = r.timestamp.timestamp().max(0);
                secs - (secs % 60)
            });
            out.push(DashboardMetrics {
                target_id: tid,
                samples,
                up,
                avg_ms,
                p50_ms,
                p95_ms,
                last_status,
                last_minute_ts,
            });
        }
        Ok(out)
    }

    async fn latest_status_by_region(
        &self,
        _org: OrgId,
        range: TimeRange,
    ) -> Result<Vec<RegionLatestStatus>> {
        // No region dimension in memory; tag every target one synthetic region.
        let guard = self.results.lock();
        let mut latest: std::collections::BTreeMap<Uuid, &CheckResult> =
            std::collections::BTreeMap::new();
        for r in guard.iter() {
            if r.timestamp < range.from || r.timestamp >= range.to {
                continue;
            }
            latest
                .entry(r.target_id)
                .and_modify(|cur| {
                    if r.timestamp >= cur.timestamp {
                        *cur = r;
                    }
                })
                .or_insert(r);
        }
        Ok(latest
            .into_iter()
            .map(|(target_id, r)| RegionLatestStatus {
                target_id,
                region: "default".to_string(),
                status: r.status,
            })
            .collect())
    }

    async fn dashboard_sparkline(
        &self,
        _org: OrgId,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
        _region: Option<&str>,
    ) -> Result<Vec<DashboardSparkBucket>> {
        let guard = self.results.lock();
        let mut buckets: std::collections::BTreeMap<(Uuid, i64), (u64, u64, u64)> =
            std::collections::BTreeMap::new();
        for r in guard.iter() {
            if r.timestamp < from || r.timestamp >= to {
                continue;
            }
            let minute_ts = (r.timestamp.timestamp() / 60) * 60;
            let slot = buckets.entry((r.target_id, minute_ts)).or_insert((0, 0, 0));
            slot.0 += r.duration_ms as u64;
            slot.1 += 1;
            if r.status == CheckStatus::Up {
                slot.2 += 1;
            }
        }
        Ok(buckets
            .into_iter()
            .map(
                |((target_id, bucket_ts), (sum, n, up))| DashboardSparkBucket {
                    target_id,
                    bucket_ts,
                    avg_ms: (sum as f32) / (n.max(1) as f32),
                    checks: n,
                    up,
                },
            )
            .collect())
    }

    async fn prior_period_summary(
        &self,
        _org: OrgId,
        range: TimeRange,
        _region: Option<&str>,
    ) -> Result<PriorPeriodSummary> {
        let guard = self.results.lock();
        let span = range.to - range.from;
        let prior_to = range.from;
        let prior_from = prior_to - span;
        let mut total = 0u64;
        let mut up = 0u64;
        let mut sum_ms: u64 = 0;
        for r in guard.iter() {
            if r.timestamp < prior_from || r.timestamp >= prior_to {
                continue;
            }
            total += 1;
            sum_ms = sum_ms.saturating_add(r.duration_ms as u64);
            if r.status == CheckStatus::Up {
                up += 1;
            }
        }
        let avg_ms = sum_ms
            .checked_div(total)
            .map_or(0, |v| v.min(u32::MAX as u64) as u32);
        Ok(PriorPeriodSummary {
            checks_total: total,
            checks_up: up,
            avg_ms,
        })
    }

    async fn latency_buckets(
        &self,
        _org: OrgId,
        target_id: Uuid,
        range: ClampedRange,
        bucket_seconds: u32,
        _region: Option<&str>,
    ) -> Result<Vec<LatencyBucket>> {
        #[derive(Default)]
        struct Acc {
            durations: Vec<u32>,
            total_sum: u64,
            // (sum, count) per phase; count skips NULL samples to match the
            // matview's `avgState(<nullable>)` semantics.
            dns: (u64, u64),
            connect: (u64, u64),
            tls: (u64, u64),
            ttfb: (u64, u64),
        }
        fn add(acc: &mut (u64, u64), v: Option<u16>) {
            if let Some(v) = v {
                acc.0 += u64::from(v);
                acc.1 += 1;
            }
        }
        let mean = |(sum, n): (u64, u64)| -> u32 {
            sum.checked_div(n)
                .map_or(0, |v| v.min(u32::MAX as u64) as u32)
        };
        // Round to a whole-minute grain exactly as the ClickHouse impl does,
        // so the in-memory test backend buckets identically to production.
        let bucket = i64::from(rollup_bucket_secs(bucket_seconds));
        let mut by_bucket: std::collections::BTreeMap<i64, Acc> = std::collections::BTreeMap::new();
        let guard = self.results.lock();
        for r in guard.iter() {
            if r.target_id != target_id || r.timestamp < range.from || r.timestamp >= range.to {
                continue;
            }
            let slot = (r.timestamp.timestamp() / bucket) * bucket;
            let acc = by_bucket.entry(slot).or_default();
            acc.durations.push(r.duration_ms);
            acc.total_sum += u64::from(r.duration_ms);
            add(&mut acc.dns, r.dns_ms);
            add(&mut acc.connect, r.connect_ms);
            add(&mut acc.tls, r.tls_ms);
            add(&mut acc.ttfb, r.ttfb_ms);
        }
        Ok(by_bucket
            .into_iter()
            .map(|(bucket_ts, mut acc)| {
                acc.durations.sort_unstable();
                let samples = acc.durations.len() as u64;
                LatencyBucket {
                    t: bucket_ts * 1000,
                    p50: percentile(&acc.durations, 0.50),
                    p95: percentile(&acc.durations, 0.95),
                    p99: percentile(&acc.durations, 0.99),
                    avg: mean((acc.total_sum, samples)),
                    dns: mean(acc.dns),
                    connect: mean(acc.connect),
                    tls: mean(acc.tls),
                    ttfb: mean(acc.ttfb),
                    samples,
                }
            })
            .collect())
    }

    async fn availability_buckets(
        &self,
        _org: OrgId,
        target_id: Uuid,
        range: ClampedRange,
        bucket_seconds: u32,
        _region: Option<&str>,
    ) -> Result<Vec<AvailabilityBucket>> {
        let bucket = i64::from(rollup_bucket_secs(bucket_seconds));
        let mut by_bucket: std::collections::BTreeMap<i64, (u64, u64)> =
            std::collections::BTreeMap::new();
        let guard = self.results.lock();
        for r in guard.iter() {
            if r.target_id != target_id || r.timestamp < range.from || r.timestamp >= range.to {
                continue;
            }
            let slot = (r.timestamp.timestamp() / bucket) * bucket;
            let acc = by_bucket.entry(slot).or_default();
            acc.0 += 1;
            if r.status == CheckStatus::Up {
                acc.1 += 1;
            }
        }
        Ok(by_bucket
            .into_iter()
            .map(|(bucket_ts, (total, up))| AvailabilityBucket {
                bucket_ts,
                total,
                up,
            })
            .collect())
    }

    async fn region_breakdown(
        &self,
        _org: OrgId,
        _target_id: Uuid,
        _range: TimeRange,
    ) -> Result<Vec<RegionRollup>> {
        Ok(Vec::new())
    }

    async fn flap_counts(
        &self,
        _org: OrgId,
        target_id: Uuid,
        range: ClampedRange,
    ) -> Result<Vec<RegionFlaps>> {
        // No region dimension in memory; count the whole series as one region.
        let guard = self.results.lock();
        let mut series: Vec<&CheckResult> = guard
            .iter()
            .filter(|r| {
                r.target_id == target_id && r.timestamp >= range.from && r.timestamp < range.to
            })
            .collect();
        if series.is_empty() {
            return Ok(Vec::new());
        }
        series.sort_by_key(|r| r.timestamp);
        let failures = series
            .iter()
            .filter(|r| r.status != CheckStatus::Up)
            .count() as u64;
        let transitions = series
            .windows(2)
            .filter(|w| w[0].status != w[1].status)
            .count() as u64;
        Ok(vec![RegionFlaps {
            region: "default".to_string(),
            failures,
            transitions,
        }])
    }

    async fn latency_buckets_by_region(
        &self,
        _org: OrgId,
        _target_id: Uuid,
        _range: ClampedRange,
        _bucket_seconds: u32,
    ) -> Result<Vec<RegionLatencySeries>> {
        Ok(Vec::new())
    }

    async fn fleet_ribbon(
        &self,
        _org: OrgId,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
        bucket_seconds: u32,
        _region: Option<&str>,
    ) -> Result<Vec<FleetRibbonBucket>> {
        let guard = self.results.lock();
        let bucket = bucket_seconds.max(60) as i64;
        let mut by_bucket: std::collections::BTreeMap<
            i64,
            (u64, u64, std::collections::BTreeSet<Uuid>),
        > = std::collections::BTreeMap::new();
        for r in guard.iter() {
            if r.timestamp < from || r.timestamp >= to {
                continue;
            }
            let slot = (r.timestamp.timestamp() / bucket) * bucket;
            let entry = by_bucket.entry(slot).or_default();
            entry.0 += 1;
            if r.status == CheckStatus::Up {
                entry.1 += 1;
            } else {
                entry.2.insert(r.target_id);
            }
        }
        Ok(by_bucket
            .into_iter()
            .map(|(bucket_ts, (samples, up, down))| FleetRibbonBucket {
                bucket_ts,
                samples,
                up,
                down_targets: down.into_iter().collect(),
            })
            .collect())
    }

    async fn last_n_summary(
        &self,
        _org: OrgId,
        range: TimeRange,
        _region: Option<&str>,
    ) -> Result<(u64, u64, u32, u64)> {
        let guard = self.results.lock();
        let mut total = 0u64;
        let mut up = 0u64;
        let mut sum_ms: u64 = 0;
        let mut by_target: std::collections::HashMap<Uuid, Vec<&CheckResult>> =
            std::collections::HashMap::new();
        for r in guard.iter() {
            if r.timestamp < range.from || r.timestamp >= range.to {
                continue;
            }
            total += 1;
            sum_ms = sum_ms.saturating_add(r.duration_ms as u64);
            if r.status == CheckStatus::Up {
                up += 1;
            }
            by_target.entry(r.target_id).or_default().push(r);
        }
        let avg_ms = sum_ms
            .checked_div(total)
            .map_or(0, |v| v.min(u32::MAX as u64) as u32);
        let mut incidents = 0u64;
        for results in by_target.values_mut() {
            results.sort_by_key(|r| r.timestamp);
            let mut in_incident = false;
            for r in results.iter() {
                let bad = matches!(r.status, CheckStatus::Down | CheckStatus::Error);
                if bad && !in_incident {
                    incidents += 1;
                    in_incident = true;
                } else if !bad {
                    in_incident = false;
                }
            }
        }
        Ok((total, up, avg_ms, incidents))
    }
}

/// Nearest-rank percentile on a pre-sorted slice. Mirrors ClickHouse's
/// `quantile(p)` so the in-memory fixture is byte-comparable to the
/// production CH impl. Empty / single-element edge cases collapse to the
/// natural answer (0 / the only value).
fn percentile(sorted: &[u32], p: f64) -> u32 {
    if sorted.is_empty() {
        return 0;
    }
    let n = sorted.len();
    let idx = ((p * n as f64).ceil() as usize)
        .saturating_sub(1)
        .min(n - 1);
    sorted[idx]
}

#[derive(Default)]
pub struct InMemoryTargetStore {
    targets: Mutex<Vec<Target>>,
}

impl InMemoryTargetStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_vec(targets: Vec<Target>) -> Self {
        Self {
            targets: Mutex::new(targets),
        }
    }

    pub fn insert(&self, target: Target) {
        self.targets.lock().push(target);
    }

    fn materialize(new: NewTarget, source: WriteSource) -> Target {
        let now = Utc::now();
        Target {
            id: Uuid::now_v7(),
            name: new.name,
            check: new.check,
            interval: new.interval,
            enabled: new.enabled,
            tags: new.tags,
            alerts: new.alerts,
            alert_confirmations: new.alert_confirmations.max(1),
            notify_recovery: new.notify_recovery,
            renotify_interval_secs: new.renotify_interval_secs,
            region_policy: new.region_policy.unwrap_or_default(),
            group_name: new.group_name,
            owner_user_id: new.owner_user_id,
            write_source: source,
            created_at: now,
            updated_at: now,
            plan_hold_at: None,
        }
    }
}

// Single-org test fixture — see the note on the `ResultsStore` impl above.
// The `org` parameter is accepted to satisfy the trait but not used for
// filtering.
#[async_trait]
impl TargetStore for InMemoryTargetStore {
    async fn list(&self, _org: OrgId, filter: TargetFilter) -> Result<Vec<Target>> {
        let limit = filter.limit.unwrap_or(100).min(10_000);
        let guard = self.targets.lock();
        let q_lower = filter
            .q
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_lowercase);
        let group = filter
            .group
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let mut collected: Vec<Target> = guard
            .iter()
            .filter(|t| filter.enabled.map(|e| t.enabled == e).unwrap_or(true))
            .filter(|t| match &filter.tag {
                Some(tag) => t.tags.iter().any(|x| x == tag),
                None => true,
            })
            .filter(|t| match &q_lower {
                Some(q) => t.name.to_lowercase().contains(q),
                None => true,
            })
            .filter(|t| match group {
                Some(g) => t.group_name.as_deref() == Some(g),
                None => true,
            })
            .filter(|t| match filter.owner {
                Some(u) => t.owner_user_id == Some(u),
                None => true,
            })
            .filter(|t| match &filter.kind {
                Some(k) => t.check.kind() == k,
                None => true,
            })
            .cloned()
            .collect();
        match filter.sort {
            crate::storage::traits::TargetSort::RecentActivity => {
                collected.sort_by_key(|t| std::cmp::Reverse(t.updated_at));
            }
            crate::storage::traits::TargetSort::Name => {
                collected.sort_by(|a, b| a.name.cmp(&b.name));
            }
            crate::storage::traits::TargetSort::Created => {
                collected.sort_by_key(|t| t.created_at);
            }
        }
        Ok(collected
            .into_iter()
            .skip(filter.offset)
            .take(limit)
            .collect())
    }

    async fn get(&self, _org: OrgId, id: Uuid) -> Result<Option<Target>> {
        Ok(self.targets.lock().iter().find(|t| t.id == id).cloned())
    }

    async fn distinct_groups(&self, _org: OrgId) -> Result<Vec<String>> {
        let mut groups: Vec<String> = self
            .targets
            .lock()
            .iter()
            .filter_map(|t| t.group_name.clone())
            .filter(|g| !g.is_empty())
            .collect();
        groups.sort();
        groups.dedup();
        Ok(groups)
    }

    async fn count_by_kind(
        &self,
        _org: OrgId,
        filter: TargetFilter,
    ) -> Result<std::collections::HashMap<String, i64>> {
        let q_lower = filter
            .q
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_lowercase);
        let group = filter
            .group
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let mut counts: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
        for t in self.targets.lock().iter() {
            if !filter.enabled.map(|e| t.enabled == e).unwrap_or(true) {
                continue;
            }
            if let Some(tag) = &filter.tag
                && !t.tags.iter().any(|x| x == tag)
            {
                continue;
            }
            if let Some(q) = &q_lower
                && !t.name.to_lowercase().contains(q)
            {
                continue;
            }
            if let Some(g) = group
                && t.group_name.as_deref() != Some(g)
            {
                continue;
            }
            if let Some(u) = filter.owner
                && t.owner_user_id != Some(u)
            {
                continue;
            }
            *counts.entry(t.check.kind().to_string()).or_default() += 1;
        }
        Ok(counts)
    }

    async fn names(&self, _org: OrgId) -> Result<std::collections::HashMap<Uuid, String>> {
        Ok(self
            .targets
            .lock()
            .iter()
            .map(|t| (t.id, t.name.clone()))
            .collect())
    }

    async fn names_and_kinds(
        &self,
        _org: OrgId,
    ) -> Result<std::collections::HashMap<Uuid, (String, String)>> {
        Ok(self
            .targets
            .lock()
            .iter()
            .map(|t| (t.id, (t.name.clone(), t.check.kind().to_string())))
            .collect())
    }

    async fn hosts_by_kind(&self, _org: OrgId, kinds: &[&str]) -> Result<Vec<(String, String)>> {
        Ok(self
            .targets
            .lock()
            .iter()
            .filter(|t| kinds.contains(&t.check.kind()))
            .filter_map(|t| {
                Some((
                    t.check.kind().to_string(),
                    t.check.primary_host()?.to_string(),
                ))
            })
            .collect())
    }

    async fn create(
        &self,
        _org: OrgId,
        new: NewTarget,
        source: WriteSource,
        max_targets: i64,
        max_flow_checks: i64,
    ) -> Result<Target> {
        let mut guard = self.targets.lock();
        // Lock held across count + push, so this is atomic for the same
        // reason the Postgres count-in-INSERT is.
        if guard.len() as i64 + 1 > max_targets {
            return Err(crate::error::AppError::quota_exceeded(
                "max_targets",
                guard.len() as i64,
                max_targets,
                "free",
            ));
        }
        if matches!(new.check, crate::domain::CheckSpec::Flow(_)) {
            let flow_current = guard
                .iter()
                .filter(|t| matches!(t.check, crate::domain::CheckSpec::Flow(_)))
                .count() as i64;
            if flow_current + 1 > max_flow_checks {
                return Err(crate::error::AppError::quota_exceeded(
                    "max_flow_checks",
                    flow_current,
                    max_flow_checks,
                    "free",
                ));
            }
        }
        let target = Self::materialize(new, source);
        guard.push(target.clone());
        Ok(target)
    }

    async fn update(
        &self,
        _org: OrgId,
        id: Uuid,
        update: TargetUpdate,
        source: Option<WriteSource>,
        _actor: Option<UserId>,
    ) -> Result<Option<Target>> {
        let mut guard = self.targets.lock();
        let Some(t) = guard.iter_mut().find(|t| t.id == id) else {
            return Ok(None);
        };
        if let Some(n) = update.name {
            t.name = n;
        }
        if let Some(c) = update.check {
            t.check = c;
        }
        if let Some(i) = update.interval {
            t.interval = i;
        }
        if let Some(e) = update.enabled {
            t.enabled = e;
        }
        if let Some(tags) = update.tags {
            t.tags = tags;
        }
        if let Some(alerts) = update.alerts {
            t.alerts = alerts;
        }
        if let Some(n) = update.alert_confirmations {
            t.alert_confirmations = n.max(1);
        }
        if let Some(r) = update.notify_recovery {
            t.notify_recovery = r;
        }
        if let Some(n) = update.renotify_interval_secs {
            t.renotify_interval_secs = n;
        }
        if let Some(p) = update.region_policy {
            t.region_policy = p;
        }
        if let Some(v) = update.group_name {
            t.group_name = v;
        }
        if let Some(v) = update.owner_user_id {
            t.owner_user_id = v;
        }
        if let Some(source) = source {
            t.write_source = source;
        }
        t.updated_at = Utc::now();
        Ok(Some(t.clone()))
    }

    async fn delete(&self, _org: OrgId, id: Uuid, _actor: Option<UserId>) -> Result<bool> {
        let mut guard = self.targets.lock();
        let before = guard.len();
        guard.retain(|t| t.id != id);
        Ok(guard.len() != before)
    }

    async fn unbind_channel(&self, _org: OrgId, channel_id: Uuid) -> Result<u64> {
        let mut guard = self.targets.lock();
        let mut touched = 0;
        for t in guard.iter_mut() {
            let before = t.alerts.0.len();
            t.alerts.0.retain(|b| b.channel_id != channel_id);
            if t.alerts.0.len() != before {
                t.updated_at = Utc::now();
                touched += 1;
            }
        }
        Ok(touched)
    }

    async fn bulk_create(
        &self,
        _org: OrgId,
        items: Vec<NewTargetWithRegions>,
        source: WriteSource,
        max_targets: i64,
        max_flow_checks: i64,
    ) -> Result<Vec<Target>> {
        let mut guard = self.targets.lock();
        if guard.len() as i64 + items.len() as i64 > max_targets {
            return Err(crate::error::AppError::quota_exceeded(
                "max_targets",
                guard.len() as i64,
                max_targets,
                "free",
            ));
        }
        let flow_len = items
            .iter()
            .filter(|i| matches!(i.target.check, crate::domain::CheckSpec::Flow(_)))
            .count() as i64;
        if flow_len > 0 {
            let flow_current = guard
                .iter()
                .filter(|t| matches!(t.check, crate::domain::CheckSpec::Flow(_)))
                .count() as i64;
            if flow_current + flow_len > max_flow_checks {
                return Err(crate::error::AppError::quota_exceeded(
                    "max_flow_checks",
                    flow_current,
                    max_flow_checks,
                    "free",
                ));
            }
        }
        let mut created = Vec::with_capacity(items.len());
        for item in items {
            let target = Self::materialize(item.target, source);
            guard.push(target.clone());
            created.push(target);
        }
        Ok(created)
    }

    async fn list_updated_since(&self, _org: OrgId, since: DateTime<Utc>) -> Result<Vec<Target>> {
        Ok(self
            .targets
            .lock()
            .iter()
            .filter(|t| t.updated_at > since)
            .cloned()
            .collect())
    }

    async fn list_tags(
        &self,
        _org: OrgId,
        prefix: Option<String>,
        limit: usize,
    ) -> Result<Vec<TagCount>> {
        let mut counts: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
        for t in self.targets.lock().iter() {
            for tag in &t.tags {
                if let Some(pfx) = prefix.as_deref()
                    && !tag.starts_with(pfx)
                {
                    continue;
                }
                *counts.entry(tag.clone()).or_default() += 1;
            }
        }
        let mut out: Vec<TagCount> = counts
            .into_iter()
            .map(|(name, count)| TagCount { name, count })
            .collect();
        out.sort_by(|a, b| b.count.cmp(&a.count).then(a.name.cmp(&b.name)));
        out.truncate(limit);
        Ok(out)
    }

    async fn summary(&self, _org: OrgId) -> Result<TargetsSummary> {
        let guard = self.targets.lock();
        let total = guard.len() as u64;
        let enabled = guard.iter().filter(|t| t.enabled).count() as u64;
        Ok(TargetsSummary {
            total,
            enabled,
            disabled: total - enabled,
        })
    }

    async fn set_enabled(
        &self,
        _org: OrgId,
        ids: &[Uuid],
        enabled: bool,
        _actor: Option<UserId>,
    ) -> Result<Vec<Uuid>> {
        let mut guard = self.targets.lock();
        let now = Utc::now();
        let mut hit = Vec::new();
        for t in guard.iter_mut() {
            if ids.contains(&t.id) {
                t.enabled = enabled;
                t.updated_at = now;
                hit.push(t.id);
            }
        }
        Ok(hit)
    }

    async fn delete_bulk(
        &self,
        _org: OrgId,
        ids: &[Uuid],
        _actor: Option<UserId>,
    ) -> Result<Vec<Uuid>> {
        let mut guard = self.targets.lock();
        let hit: Vec<Uuid> = guard
            .iter()
            .filter(|t| ids.contains(&t.id))
            .map(|t| t.id)
            .collect();
        guard.retain(|t| !ids.contains(&t.id));
        Ok(hit)
    }

    async fn add_tags(&self, _org: OrgId, ids: &[Uuid], tags: &[String]) -> Result<TagAddOutcome> {
        let mut guard = self.targets.lock();
        let now = Utc::now();
        let mut outcome = TagAddOutcome::default();
        for t in guard.iter_mut() {
            if !ids.contains(&t.id) {
                continue;
            }
            let mut merged = t.tags.clone();
            for tag in tags {
                if !merged.iter().any(|x| x == tag) {
                    merged.push(tag.clone());
                }
            }
            if merged.len() > MAX_TAGS_PER_TARGET {
                outcome.over_cap.push(t.id);
                continue;
            }
            t.tags = merged;
            t.updated_at = now;
            outcome.updated.push(t.id);
        }
        Ok(outcome)
    }

    async fn remove_tags(&self, _org: OrgId, ids: &[Uuid], tags: &[String]) -> Result<Vec<Uuid>> {
        let mut guard = self.targets.lock();
        let now = Utc::now();
        let mut hit = Vec::new();
        for t in guard.iter_mut() {
            if ids.contains(&t.id) {
                t.tags.retain(|x| !tags.contains(x));
                t.updated_at = now;
                hit.push(t.id);
            }
        }
        Ok(hit)
    }

    async fn set_group(&self, _org: OrgId, ids: &[Uuid], group: Option<&str>) -> Result<Vec<Uuid>> {
        let mut guard = self.targets.lock();
        let now = Utc::now();
        let mut hit = Vec::new();
        for t in guard.iter_mut() {
            if ids.contains(&t.id) {
                t.group_name = group.map(str::to_owned);
                t.updated_at = now;
                hit.push(t.id);
            }
        }
        Ok(hit)
    }

    async fn ping(&self) -> Result<()> {
        Ok(())
    }

    async fn regions_for_org(&self, _org: OrgId) -> Result<Vec<String>> {
        Ok(Vec::new())
    }

    async fn available_regions(&self) -> Result<Vec<String>> {
        Ok(Vec::new())
    }

    async fn available_regions_detailed(&self) -> Result<Vec<RegionOption>> {
        Ok(Vec::new())
    }

    async fn default_selected_regions(&self) -> Result<Vec<String>> {
        Ok(Vec::new())
    }

    async fn regions_for_target(
        &self,
        _org: OrgId,
        target_id: Uuid,
    ) -> Result<Option<Vec<String>>> {
        let exists = self.targets.lock().iter().any(|t| t.id == target_id);
        Ok(exists.then(Vec::new))
    }

    async fn flow_capable_regions(&self) -> Result<Vec<String>> {
        Ok(Vec::new())
    }

    async fn set_target_regions(
        &self,
        _org: OrgId,
        target_id: Uuid,
        _regions: &[String],
    ) -> Result<bool> {
        Ok(self.targets.lock().iter().any(|t| t.id == target_id))
    }
}

#[async_trait]
impl crate::storage::admin::EnabledTargetSource for InMemoryTargetStore {
    /// Single-org test fixture: org is not modelled, so every enabled target
    /// is tagged with the nil-UUID org.
    async fn list_all_enabled_targets(&self) -> Result<Vec<(OrgId, Target)>> {
        let org = OrgId(uuid::Uuid::nil());
        Ok(self
            .targets
            .lock()
            .iter()
            .filter(|t| t.enabled)
            .map(|t| (org, t.clone()))
            .collect())
    }
}

#[async_trait]
impl crate::storage::admin::EnabledTargetStream for InMemoryTargetStore {
    async fn next_enabled_target_page(
        &self,
        after: Option<crate::storage::admin::PublicTargetCursor>,
        limit: usize,
    ) -> Result<Vec<(OrgId, Target)>> {
        let org = OrgId(uuid::Uuid::nil());
        // Single-org fixture: every row's org is the nil sentinel, so the
        // `(org, id) > cursor` tuple compare collapses to `id > cursor.id`.
        let cursor_target = after.map(|c| c.target_id);
        let mut hits: Vec<Target> = self
            .targets
            .lock()
            .iter()
            // In-memory fixture has no page model; the writer watches every
            // enabled target (page-membership filtering is a PG-only concern).
            .filter(|t| t.enabled)
            .filter(|t| cursor_target.is_none_or(|cid| t.id > cid))
            .cloned()
            .collect();
        hits.sort_by_key(|t| t.id);
        hits.truncate(limit);
        Ok(hits.into_iter().map(|t| (org, t)).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    /// A bulk add merges into what each monitor already carries, so a legal
    /// request can still produce an illegal list. The cap lands on the result,
    /// and the monitor it skipped is reported rather than called missing.
    #[tokio::test]
    async fn a_bulk_tag_add_stops_at_the_cap_and_names_the_monitor_it_skipped() {
        use crate::domain::CheckSpec;
        use crate::domain::target::MAX_TAGS_PER_TARGET;

        let store = InMemoryTargetStore::new();
        let org = OrgId(Uuid::nil());
        let seed = |tags: Vec<String>| NewTarget {
            name: "m".into(),
            check: CheckSpec::Tcp(crate::domain::TcpCheck {
                host: "example.com".into(),
                port: 443,
                timeout: std::time::Duration::from_secs(3),
            }),
            interval: std::time::Duration::from_secs(60),
            enabled: true,
            tags,
            alerts: Default::default(),
            region_policy: None,
            alert_confirmations: 2,
            notify_recovery: true,
            renotify_interval_secs: 3600,
            group_name: None,
            owner_user_id: None,
            regions: None,
        };

        let full: Vec<String> = (0..MAX_TAGS_PER_TARGET).map(|i| format!("t{i}")).collect();
        let crowded = store
            .create(org, seed(full), WriteSource::Ui, i64::MAX, i64::MAX)
            .await
            .unwrap()
            .id;
        let roomy = store
            .create(
                org,
                seed(vec!["prod".into()]),
                WriteSource::Ui,
                i64::MAX,
                i64::MAX,
            )
            .await
            .unwrap()
            .id;

        let outcome = store
            .add_tags(org, &[crowded, roomy], &["fresh".to_string()])
            .await
            .unwrap();
        assert_eq!(outcome.updated, vec![roomy]);
        assert_eq!(outcome.over_cap, vec![crowded]);
        let untouched = store.get(org, crowded).await.unwrap().unwrap();
        assert_eq!(untouched.tags.len(), MAX_TAGS_PER_TARGET);
        assert!(!untouched.tags.iter().any(|t| t == "fresh"));

        // A tag it already carries is not a new one, so a full monitor takes it.
        let outcome = store
            .add_tags(org, &[crowded], &["t0".to_string()])
            .await
            .unwrap();
        assert_eq!(outcome.updated, vec![crowded]);
        assert!(outcome.over_cap.is_empty());
    }

    fn up_result(
        target: Uuid,
        base: DateTime<Utc>,
        offset_s: i64,
        dur: u32,
        dns: u16,
    ) -> CheckResult {
        CheckResult {
            target_id: target,
            org_id: Uuid::nil(),
            timestamp: base + chrono::Duration::seconds(offset_s),
            status: CheckStatus::Up,
            duration_ms: dur,
            dns_ms: Some(dns),
            connect_ms: Some(10),
            tls_ms: None, // exercises the avgState-skips-NULL path
            ttfb_ms: Some(20),
            response_code: Some(200),
            response_size: None,
            diagnostic: None,
            error: None,
        }
    }

    #[tokio::test]
    async fn latency_buckets_aggregate_per_slice_and_skip_null_phases() {
        let store = InMemorySink::new();
        let t = Uuid::from_u128(1);
        let base = Utc.with_ymd_and_hms(2026, 5, 1, 0, 0, 0).unwrap();
        // Two samples in slice 0 (0s, 30s), one in slice 1 (60s).
        store
            .write_batch(&[
                up_result(t, base, 0, 100, 10),
                up_result(t, base, 30, 200, 30),
                up_result(t, base, 60, 300, 50),
            ])
            .await
            .unwrap();
        let range = TimeRange {
            from: base,
            to: base + chrono::Duration::seconds(120),
        };
        let buckets = store
            .latency_buckets(
                OrgId(Uuid::nil()),
                t,
                ClampedRange::unclamped(range),
                60,
                None,
            )
            .await
            .unwrap();

        assert_eq!(buckets.len(), 2);
        let b0 = &buckets[0];
        assert_eq!(b0.t, base.timestamp() * 1000, "t is unix-millis");
        assert_eq!(b0.samples, 2);
        assert_eq!(b0.avg, 150); // (100+200)/2
        assert_eq!(b0.dns, 20); // (10+30)/2
        assert_eq!(b0.tls, 0); // every sample NULL → skipped, finalises to 0
        assert_eq!(buckets[1].samples, 1);
        assert_eq!(buckets[1].p50, 300);
    }

    #[tokio::test]
    async fn latency_buckets_isolate_target_and_window() {
        let store = InMemorySink::new();
        let t = Uuid::from_u128(1);
        let base = Utc.with_ymd_and_hms(2026, 5, 1, 0, 0, 0).unwrap();
        store
            .write_batch(&[
                up_result(t, base, 0, 100, 10),
                up_result(Uuid::from_u128(2), base, 0, 100, 10), // other target
                up_result(t, base, -120, 100, 10),               // before window
            ])
            .await
            .unwrap();
        let range = TimeRange {
            from: base,
            to: base + chrono::Duration::seconds(60),
        };
        let buckets = store
            .latency_buckets(
                OrgId(Uuid::nil()),
                t,
                ClampedRange::unclamped(range),
                60,
                None,
            )
            .await
            .unwrap();
        assert_eq!(buckets.len(), 1);
        assert_eq!(buckets[0].samples, 1);
    }

    #[tokio::test]
    async fn availability_buckets_up_ratio_per_slice() {
        let store = InMemorySink::new();
        let t = Uuid::from_u128(1);
        let base = Utc.with_ymd_and_hms(2026, 5, 1, 0, 0, 0).unwrap();
        let down = |offset_s: i64| CheckResult {
            status: CheckStatus::Down,
            ..up_result(t, base, offset_s, 100, 10)
        };
        // Slice 0: 2 up + 1 down. Slice 1: 1 up.
        store
            .write_batch(&[
                up_result(t, base, 0, 100, 10),
                up_result(t, base, 30, 100, 10),
                down(45),
                up_result(t, base, 60, 100, 10),
            ])
            .await
            .unwrap();
        let range = TimeRange {
            from: base,
            to: base + chrono::Duration::seconds(120),
        };
        let buckets = store
            .availability_buckets(
                OrgId(Uuid::nil()),
                t,
                ClampedRange::unclamped(range),
                60,
                None,
            )
            .await
            .unwrap();

        assert_eq!(buckets.len(), 2);
        assert_eq!(buckets[0].bucket_ts, base.timestamp());
        assert_eq!(buckets[0].total, 3);
        assert_eq!(buckets[0].up, 2);
        assert_eq!(buckets[1].total, 1);
        assert_eq!(buckets[1].up, 1);
    }

    #[tokio::test]
    async fn availability_buckets_isolate_target_and_window() {
        let store = InMemorySink::new();
        let t = Uuid::from_u128(1);
        let base = Utc.with_ymd_and_hms(2026, 5, 1, 0, 0, 0).unwrap();
        store
            .write_batch(&[
                up_result(t, base, 0, 100, 10),
                up_result(Uuid::from_u128(2), base, 0, 100, 10), // other target
                up_result(t, base, -120, 100, 10),               // before window
            ])
            .await
            .unwrap();
        let range = TimeRange {
            from: base,
            to: base + chrono::Duration::seconds(60),
        };
        let buckets = store
            .availability_buckets(
                OrgId(Uuid::nil()),
                t,
                ClampedRange::unclamped(range),
                60,
                None,
            )
            .await
            .unwrap();
        assert_eq!(buckets.len(), 1);
        assert_eq!(buckets[0].total, 1);
        assert_eq!(buckets[0].up, 1);
    }

    #[tokio::test]
    async fn list_failures_by_region_excludes_up_and_paginates() {
        let store = InMemorySink::new();
        let t = Uuid::from_u128(1);
        let base = Utc.with_ymd_and_hms(2026, 5, 1, 0, 0, 0).unwrap();
        let fail = |offset_s: i64, status: CheckStatus| status_result(t, base, offset_s, status);
        store
            .write_batch(&[
                up_result(t, base, 0, 100, 10),
                fail(30, CheckStatus::Down),
                up_result(t, base, 45, 100, 10),
                fail(60, CheckStatus::Error),
                fail(90, CheckStatus::Degraded),
            ])
            .await
            .unwrap();
        let range = ClampedRange::unclamped(TimeRange {
            from: base,
            to: base + chrono::Duration::seconds(120),
        });

        let page1 = store
            .list_failures_by_region(OrgId(Uuid::nil()), t, range, 2, 0, None)
            .await
            .unwrap();
        // Only the three non-Up checks, newest first, bounded to the page size.
        assert_eq!(page1.len(), 2);
        assert!(page1.iter().all(|(_, r)| r.status != CheckStatus::Up));
        assert_eq!(page1[0].1.status, CheckStatus::Degraded); // 90s, newest
        assert_eq!(page1[1].1.status, CheckStatus::Error); // 60s

        let page2 = store
            .list_failures_by_region(OrgId(Uuid::nil()), t, range, 2, 2, None)
            .await
            .unwrap();
        assert_eq!(page2.len(), 1);
        assert_eq!(page2[0].1.status, CheckStatus::Down); // 30s, oldest failure
    }

    fn status_result(
        target: Uuid,
        base: DateTime<Utc>,
        offset_s: i64,
        status: CheckStatus,
    ) -> CheckResult {
        CheckResult {
            status,
            ..up_result(target, base, offset_s, 100, 10)
        }
    }
}
