use std::collections::{HashMap, HashSet};
use std::net::Ipv6Addr;
use std::time::Duration;

use anyhow::Context;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use clickhouse::{Client, Row};
use serde::Deserialize;
use uuid::Uuid;

use crate::domain::agent_wire::{ConsoleLine, FlowEvidence, StepOutcome, StepTrace};
use crate::domain::metrics::{
    AvailabilityBucket, DashboardMetrics, DashboardSparkBucket, FleetRibbonBucket, FlowStepBucket,
    FlowStepTrend, LatencyBucket, PriorPeriodSummary, RegionLatencySeries, RegionRollup,
};
use crate::domain::{
    CheckDetails, CheckDiagnostic, CheckDiagnosticKind, CheckOutcome, CheckResult, CheckStatus,
    DiagnosticConfidence, DiagnosticEvidence, DnsAnswer, EdgeProvider, Incident, NetworkPath,
    ObservedCadence, OrgId, coalesce_incidents,
};
use crate::error::Result;
use crate::storage::traits::{
    ClampedRange, FailingServer, FlowRunView, PassMarks, RecentOutcomes, RegionFlaps,
    RegionLatestStatus, RegionNewest, RegionOutcome, ResultsStore, TimeRange, UptimeStats,
    rollup_bucket_secs,
};

use super::source::{Source, bind_window};
use super::{
    FLOW_TABLE, HEARTBEAT_PING_TABLE, TABLE, fetch_single, from_unix_secs, past_evidence,
    to_unix_secs,
};
use crate::storage::org_ttl::OrgTtlDays;

/// Org-scoped read side of the ClickHouse results table. Holds no ambient
/// org; every query binds the `org` the caller resolved from the request, so
/// a `target_id` guessed from another tenant returns zero rows. `org_id` is
/// also the leading sort key — filtering on it is mandatory for performance,
/// not only isolation.
pub struct ClickhouseResultsStore {
    client: Client,
    retention: OrgTtlDays,
}

impl ClickhouseResultsStore {
    pub fn from_client(client: Client) -> Self {
        Self {
            client,
            retention: OrgTtlDays::default(),
        }
    }

    /// The snapshot that stamps each row's TTL, so reads stop at the same
    /// window the rows were kept for.
    pub fn with_retention(mut self, retention: OrgTtlDays) -> Self {
        self.retention = retention;
        self
    }

    fn source(&self, org: OrgId, from: DateTime<Utc>) -> Source {
        Source::for_range(from, self.retention.days_for(org.0).row)
    }
}

#[derive(Debug, Row, Deserialize)]
struct CadenceRow {
    samples: u64,
    median_gap_secs: u32,
}

#[derive(Debug, Row, Deserialize)]
struct StoredFlowRunRow {
    timestamp: u32,
    region: String,
    status: i8,
    duration_ms: u32,
    stopped_step: Option<u16>,
    error: String,
    step_op: Vec<String>,
    step_outcome: Vec<i8>,
    step_ms: Vec<u32>,
    final_url: String,
    title: String,
    text_snippet: String,
    console_level: Vec<String>,
    console_text: Vec<String>,
    evidence_expired: u8,
}

/// An expired evidence column reads empty, so it maps back to absent rather
/// than to a blank string the page would render as real.
fn some_if_filled(s: String) -> Option<String> {
    (!s.is_empty()).then_some(s)
}

impl From<StoredFlowRunRow> for FlowRunView {
    fn from(r: StoredFlowRunRow) -> Self {
        let steps = r
            .step_op
            .into_iter()
            .zip(r.step_outcome)
            .zip(r.step_ms)
            .map(|((op, outcome), duration_ms)| StepTrace {
                op,
                outcome: StepOutcome::from_enum8(outcome),
                duration_ms,
            })
            .collect();
        let console: Vec<ConsoleLine> = r
            .console_level
            .into_iter()
            .zip(r.console_text)
            .map(|(level, text)| ConsoleLine { level, text })
            .collect();
        let final_url = some_if_filled(r.final_url);
        let title = some_if_filled(r.title);
        let text_snippet = some_if_filled(r.text_snippet);
        let has_page =
            final_url.is_some() || title.is_some() || text_snippet.is_some() || !console.is_empty();
        // The column TTL empties the page on a background merge, not when the
        // window closes, so the window decides — never what is still there.
        let past_window = r.evidence_expired != 0;
        Self {
            timestamp: from_unix_secs(r.timestamp),
            region: r.region,
            status: CheckStatus::from_enum8(r.status),
            duration_ms: r.duration_ms,
            stopped_step: r.stopped_step.map(usize::from),
            error: some_if_filled(r.error),
            steps,
            // Only a run that stopped at a step ever captured one to lose.
            evidence_expired: past_window && r.stopped_step.is_some(),
            evidence: (has_page && !past_window).then_some(FlowEvidence {
                final_url,
                title,
                text_snippet,
                console,
            }),
        }
    }
}

#[derive(Debug, Row, Deserialize)]
struct IncidentRow {
    #[serde(with = "clickhouse::serde::uuid")]
    target_id: Uuid,
    timestamp: u32,
    status: i8,
    error: Option<String>,
}

fn coalesce_from_incident_rows(target_id: Uuid, rows: Vec<IncidentRow>) -> Vec<Incident> {
    coalesce_incidents(
        target_id,
        rows.into_iter().map(|r| {
            (
                from_unix_secs(r.timestamp),
                CheckStatus::from_enum8(r.status),
                r.error,
            )
        }),
    )
}

/// Shared tail of every raw-result read, in [`OwnedResultRow`] field order.
/// Reads decoded into a row with `details` select it after these.
const RESULT_COLUMNS: &str = "timestamp, status, duration_ms, dns_ms, connect_ms, tls_ms, \
     ttfb_ms, response_code, response_size, remote_ip, tried_ips, hop_url, dns_resolver, \
     dns_answer, dns_ttl, error, diagnostic_kind, diagnostic_confidence, diagnostic_provider, \
     diagnostic_evidence";

#[derive(Debug, Row, Deserialize)]
struct OwnedResultRow {
    #[serde(with = "clickhouse::serde::uuid")]
    target_id: Uuid,
    timestamp: u32,
    status: i8,
    duration_ms: u32,
    dns_ms: Option<u16>,
    connect_ms: Option<u16>,
    tls_ms: Option<u16>,
    ttfb_ms: Option<u16>,
    response_code: Option<u16>,
    response_size: Option<u32>,
    remote_ip: Option<Ipv6Addr>,
    tried_ips: Vec<Ipv6Addr>,
    hop_url: Option<String>,
    dns_resolver: Option<Ipv6Addr>,
    dns_answer: Vec<Ipv6Addr>,
    dns_ttl: Option<u32>,
    error: Option<String>,
    diagnostic_kind: Option<String>,
    diagnostic_confidence: Option<String>,
    diagnostic_provider: Option<String>,
    diagnostic_evidence: Vec<String>,
    details: Option<String>,
}

#[derive(Debug, Row, Deserialize)]
struct RegionResultRow {
    region: String,
    #[serde(with = "clickhouse::serde::uuid")]
    target_id: Uuid,
    timestamp: u32,
    status: i8,
    duration_ms: u32,
    dns_ms: Option<u16>,
    connect_ms: Option<u16>,
    tls_ms: Option<u16>,
    ttfb_ms: Option<u16>,
    response_code: Option<u16>,
    response_size: Option<u32>,
    remote_ip: Option<Ipv6Addr>,
    tried_ips: Vec<Ipv6Addr>,
    hop_url: Option<String>,
    dns_resolver: Option<Ipv6Addr>,
    dns_answer: Vec<Ipv6Addr>,
    dns_ttl: Option<u32>,
    error: Option<String>,
    diagnostic_kind: Option<String>,
    diagnostic_confidence: Option<String>,
    diagnostic_provider: Option<String>,
    diagnostic_evidence: Vec<String>,
    details: Option<String>,
}

impl RegionResultRow {
    fn split(self, org_id: Uuid) -> (String, CheckResult) {
        let inner = OwnedResultRow {
            target_id: self.target_id,
            timestamp: self.timestamp,
            status: self.status,
            duration_ms: self.duration_ms,
            dns_ms: self.dns_ms,
            connect_ms: self.connect_ms,
            tls_ms: self.tls_ms,
            ttfb_ms: self.ttfb_ms,
            response_code: self.response_code,
            response_size: self.response_size,
            remote_ip: self.remote_ip,
            tried_ips: self.tried_ips,
            hop_url: self.hop_url,
            dns_resolver: self.dns_resolver,
            dns_answer: self.dns_answer,
            dns_ttl: self.dns_ttl,
            error: self.error,
            diagnostic_kind: self.diagnostic_kind,
            diagnostic_confidence: self.diagnostic_confidence,
            diagnostic_provider: self.diagnostic_provider,
            diagnostic_evidence: self.diagnostic_evidence,
            details: self.details,
        };
        (self.region, row_to_result(inner, org_id))
    }
}

/// The verdict and its cause only: the incident writer reads many targets on a
/// short loop and uses nothing else.
#[derive(Debug, Row, Deserialize)]
struct OutcomeRow {
    region: String,
    #[serde(with = "clickhouse::serde::uuid")]
    target_id: Uuid,
    timestamp: u32,
    ingested_at: u32,
    status: i8,
    error: Option<String>,
    diagnostic_kind: Option<String>,
    diagnostic_confidence: Option<String>,
    diagnostic_provider: Option<String>,
    diagnostic_evidence: Vec<String>,
}

impl OutcomeRow {
    fn split(self) -> RegionOutcome {
        let diagnostic = diagnostic_from(
            self.diagnostic_kind.as_deref(),
            self.diagnostic_confidence.as_deref(),
            self.diagnostic_provider.as_deref(),
            &self.diagnostic_evidence,
        );
        let outcome = CheckOutcome {
            target_id: self.target_id,
            timestamp: from_unix_secs(self.timestamp),
            status: CheckStatus::from_enum8(self.status),
            error: self.error,
            diagnostic,
        };
        RegionOutcome {
            region: self.region,
            outcome,
            arrival: u64::from(self.ingested_at),
        }
    }
}

fn network_from(row: &OwnedResultRow) -> Option<NetworkPath> {
    let ips = |v: &[Ipv6Addr]| v.iter().map(|ip| ip.to_canonical()).collect();
    let path = NetworkPath {
        remote_ip: row.remote_ip.map(|ip| ip.to_canonical()),
        tried_ips: ips(&row.tried_ips),
        hop_url: row.hop_url.clone(),
        dns: row
            .dns_resolver
            .zip(row.dns_ttl)
            .map(|(resolver, ttl_secs)| DnsAnswer {
                resolver: resolver.to_canonical(),
                addresses: ips(&row.dns_answer),
                ttl_secs,
            }),
    };
    Some(path).filter(|p| !p.is_empty())
}

fn diagnostic_from(
    kind: Option<&str>,
    confidence: Option<&str>,
    provider: Option<&str>,
    evidence: &[String],
) -> Option<CheckDiagnostic> {
    let kind = CheckDiagnosticKind::parse(kind?)?;
    Some(CheckDiagnostic {
        kind,
        confidence: confidence
            .and_then(DiagnosticConfidence::parse)
            .unwrap_or(DiagnosticConfidence::Medium),
        provider: provider.and_then(EdgeProvider::parse),
        evidence: evidence
            .iter()
            .filter_map(|item| DiagnosticEvidence::parse(item))
            .collect(),
        // Derived so an old row cannot keep advice the product dropped.
        remediations: kind.remediations(),
    })
}

fn row_to_result(row: OwnedResultRow, org_id: Uuid) -> CheckResult {
    let diagnostic = diagnostic_from(
        row.diagnostic_kind.as_deref(),
        row.diagnostic_confidence.as_deref(),
        row.diagnostic_provider.as_deref(),
        &row.diagnostic_evidence,
    );
    CheckResult {
        target_id: row.target_id,
        org_id,
        timestamp: from_unix_secs(row.timestamp),
        status: CheckStatus::from_enum8(row.status),
        duration_ms: row.duration_ms,
        dns_ms: row.dns_ms,
        connect_ms: row.connect_ms,
        tls_ms: row.tls_ms,
        ttfb_ms: row.ttfb_ms,
        response_code: row.response_code,
        response_size: row.response_size,
        network: network_from(&row),
        diagnostic,
        error: row.error,
        details: row.details.as_deref().and_then(CheckDetails::parse),
    }
}

/// About 80 bytes a pair, so a query stays far below ClickHouse's
/// `max_query_size` (256 KiB by default) however many targets are asked about.
const PAIRS_PER_QUERY: usize = 500;

/// `(org_id, target_id)` tuples for an IN clause. Typed ids, so the literal
/// list is injection-safe; it hits the sort-key prefix so CH prunes by org
/// instead of scanning every tenant's granules.
fn pair_list(targets: &[(OrgId, Uuid)]) -> String {
    targets
        .iter()
        .map(|(o, t)| format!("('{}','{}')", o.0, t))
        .collect::<Vec<_>>()
        .join(",")
}

#[async_trait]
impl RecentOutcomes for ClickhouseResultsStore {
    async fn recent_outcomes(
        &self,
        targets: &[(OrgId, Uuid)],
        since: DateTime<Utc>,
        arrived_within: Option<Duration>,
        per_target_limit: usize,
    ) -> Result<Vec<RegionOutcome>> {
        if targets.is_empty() {
            return Ok(Vec::new());
        }
        let limit = per_target_limit.min(10_000) as u64;
        let pair_list = pair_list(targets);
        // The server's own clock on both sides, so neither ours nor an
        // agent's has to agree with it. The minmax index on `ingested_at`
        // drops every granule nothing has landed in since.
        let arrived = arrived_within
            .map(|_| "AND ingested_at >= now() - toIntervalSecond(?)")
            .unwrap_or("");
        let mut q = self
            .client
            .query(&format!(
                "SELECT region, target_id, timestamp, ingested_at, status, error, \
                 diagnostic_kind, diagnostic_confidence, diagnostic_provider, \
                 diagnostic_evidence FROM {TABLE} \
                 WHERE (org_id, target_id) IN ({pair_list}) \
                 AND timestamp >= fromUnixTimestamp(?) {arrived} \
                 ORDER BY target_id, region, timestamp DESC \
                 LIMIT {limit} BY target_id, region"
            ))
            .bind(since.timestamp());
        if let Some(within) = arrived_within {
            q = q.bind(within.as_secs() + 1);
        }
        let rows: Vec<OutcomeRow> = q
            .fetch_all::<OutcomeRow>()
            .await
            .context("clickhouse recent_outcomes")?;
        Ok(rows.into_iter().map(OutcomeRow::split).collect())
    }
}

#[async_trait]
impl ResultsStore for ClickhouseResultsStore {
    async fn flow_runs(
        &self,
        org: OrgId,
        target_id: Uuid,
        range: ClampedRange,
        region: Option<&str>,
        limit: usize,
    ) -> Result<Vec<FlowRunView>> {
        let limit = limit.min(500) as u64;
        let region_clause = if region.is_some() {
            "AND region = ? "
        } else {
            ""
        };
        // The newest runs answer "is it healthy now"; the newest failures answer
        // "what went wrong", and at the interval floor those are rarely the same
        // rows — a page of newest-only reaches back hours while the table holds
        // weeks. UNION DISTINCT folds a run that is both into one row.
        // The page columns read empty both when they expired and when the run
        // never captured one, so each branch decides which from the window the
        // row itself carries. The outer select reads that answer back by name;
        // `evidence_days` is not projected, so it cannot recompute it.
        let cols = "timestamp, region, status, duration_ms, stopped_step, error, \
             step_op, step_outcome, step_ms, final_url, title, text_snippet, \
             console_level, console_text";
        let branch_cols = format!("{cols}, {} AS evidence_expired", past_evidence("timestamp"));
        let outer_cols = format!("{cols}, evidence_expired");
        let scope = format!(
            "org_id = ? AND target_id = ? \
             AND timestamp >= fromUnixTimestamp(?) AND timestamp < fromUnixTimestamp(?) \
             {region_clause}"
        );
        let mut q = self.client.query(&format!(
            "SELECT {outer_cols} FROM (\
                 SELECT {branch_cols} FROM {FLOW_TABLE} WHERE {scope} \
                 ORDER BY timestamp DESC LIMIT {limit} \
                 UNION DISTINCT \
                 SELECT {branch_cols} FROM {FLOW_TABLE} WHERE {scope} AND status != 'up' \
                 ORDER BY timestamp DESC LIMIT {limit}\
             ) ORDER BY timestamp DESC"
        ));
        // Bound twice: once per branch of the union, in source order.
        for _ in 0..2 {
            q = q
                .bind(org.0)
                .bind(target_id)
                .bind(range.from.timestamp())
                .bind(range.to.timestamp());
            if let Some(r) = region {
                q = q.bind(r);
            }
        }
        let rows: Vec<StoredFlowRunRow> = q
            .fetch_all()
            .await
            .context("clickhouse flow_runs for target")?;
        Ok(rows.into_iter().map(FlowRunView::from).collect())
    }

    async fn flow_step_buckets(
        &self,
        org: OrgId,
        target_id: Uuid,
        range: ClampedRange,
        bucket_seconds: u32,
        region: Option<&str>,
    ) -> Result<Vec<FlowStepTrend>> {
        #[derive(Row, Deserialize)]
        struct StepRow {
            step: u16,
            op: String,
            bucket_ts: u32,
            /// `NaN` when the bucket holds no passing run — `avgIf` over an
            /// empty match has no mean to report.
            avg_ms: f64,
            samples: u64,
            failed: u64,
        }
        // Not the whole-minute grain the other bucketed reads snap to: this one
        // has no neighbouring chart to line up with. A floor is still needed: a
        // zero-second interval has no start to round to.
        let bucket = bucket_seconds.max(60);
        let region_pred = region.map(|_| "AND region = ?").unwrap_or("");
        // `arrayEnumerate` fans one run out to a row per step, and the index
        // it yields is the step's own index. A bucket survives on failures
        // alone so the reader can tell a step that only ever fails from one
        // the journey never reached.
        let query = format!(
            "SELECT toUInt16(idx - 1) AS step, \
                    argMax(op, ts) AS op, \
                    toUInt32(toStartOfInterval(ts, INTERVAL {bucket} SECOND)) AS bucket_ts, \
                    avgIf(ms, outcome = 'passed') AS avg_ms, \
                    countIf(outcome = 'passed') AS samples, \
                    countIf(outcome = 'failed') AS failed \
             FROM (\
                 SELECT timestamp AS ts, \
                        arrayJoin(arrayEnumerate(step_ms)) AS idx, \
                        step_op[idx] AS op, \
                        step_outcome[idx] AS outcome, \
                        step_ms[idx] AS ms \
                 FROM {FLOW_TABLE} \
                 WHERE org_id = ? AND target_id = ? {region_pred} \
                   AND timestamp >= fromUnixTimestamp(?) AND timestamp < fromUnixTimestamp(?)\
             ) \
             WHERE outcome != 'skipped' \
             GROUP BY step, bucket_ts \
             ORDER BY step, bucket_ts"
        );
        let mut q = self.client.query(&query).bind(org.0).bind(target_id);
        if let Some(r) = region {
            q = q.bind(r);
        }
        let rows: Vec<StepRow> = q
            .bind(range.from.timestamp())
            .bind(range.to.timestamp())
            .fetch_all()
            .await
            .context("clickhouse flow_step_buckets")?;

        // Ordered by step then time, so one pass folds without a map.
        let mut trends: Vec<FlowStepTrend> = Vec::new();
        for r in rows {
            let bucket = FlowStepBucket {
                t: i64::from(r.bucket_ts) * 1000,
                avg: (!r.avg_ms.is_nan()).then(|| r.avg_ms.round().max(0.0) as u32),
                samples: r.samples,
                failed: r.failed,
            };
            match trends.last_mut() {
                // Oldest bucket first, and each carries its own newest op, so
                // the label lands on what the step runs today.
                Some(t) if t.step == r.step => {
                    t.op = r.op;
                    t.buckets.push(bucket);
                }
                _ => trends.push(FlowStepTrend {
                    step: r.step,
                    op: r.op,
                    buckets: vec![bucket],
                }),
            }
        }
        Ok(trends)
    }

    async fn heartbeat_failure_output(
        &self,
        org: OrgId,
        target_id: Uuid,
        at: DateTime<Utc>,
    ) -> Result<Option<String>> {
        // `received_at` is second-granularity; Postgres microseconds must go.
        let body: Vec<String> = self
            .client
            .query(&format!(
                "SELECT if({expired}, '', body) \
                 FROM {HEARTBEAT_PING_TABLE} \
                 WHERE org_id = ? AND target_id = ? AND signal = 'fail' \
                   AND received_at = fromUnixTimestamp(?) LIMIT 1",
                expired = past_evidence("received_at"),
            ))
            .bind(org.0)
            .bind(target_id)
            .bind(to_unix_secs(at))
            .fetch_all::<String>()
            .await
            .context("clickhouse heartbeat failure output")?;
        // Past its window it reads empty, the same as a ping that carried none.
        Ok(body.into_iter().next().filter(|b| !b.is_empty()))
    }

    async fn heartbeat_ping_count(
        &self,
        org: OrgId,
        target_id: Uuid,
        range: ClampedRange,
    ) -> Result<Option<u64>> {
        let counts: Vec<u64> = self
            .client
            .query(&format!(
                "SELECT count() FROM {HEARTBEAT_PING_TABLE} \
                 WHERE org_id = ? AND target_id = ? AND signal != 'start' \
                   AND received_at >= fromUnixTimestamp(?) \
                   AND received_at < fromUnixTimestamp(?)"
            ))
            .bind(org.0)
            .bind(target_id)
            .bind(to_unix_secs(range.from))
            .bind(to_unix_secs(range.to))
            .fetch_all::<u64>()
            .await
            .context("clickhouse heartbeat ping count")?;
        Ok(counts.into_iter().next())
    }

    async fn heartbeat_cadence(
        &self,
        org: OrgId,
        target_id: Uuid,
        days: u16,
    ) -> Result<Option<ObservedCadence>> {
        // Successes only: a `/start` says a run began, not that the schedule
        // came round. The first row has no predecessor, so drop its epoch lag.
        let rows: Vec<CadenceRow> = self
            .client
            .query(&format!(
                "SELECT count() AS samples, \
                        toUInt32(quantileExact(0.5)(gap)) AS median_gap_secs \
                 FROM ( \
                     SELECT dateDiff('second', prev, received_at) AS gap FROM ( \
                         SELECT received_at, lagInFrame(received_at) OVER ( \
                                    ORDER BY received_at ASC \
                                    ROWS BETWEEN 1 PRECEDING AND CURRENT ROW) AS prev \
                         FROM {HEARTBEAT_PING_TABLE} \
                         WHERE org_id = ? AND target_id = ? AND signal = 'success' \
                           AND received_at >= now() - toIntervalDay(?) \
                     ) WHERE prev > toDateTime(0) \
                 )"
            ))
            .bind(org.0)
            .bind(target_id)
            .bind(days)
            .fetch_all::<CadenceRow>()
            .await
            .context("clickhouse heartbeat cadence")?;
        Ok(rows
            .into_iter()
            .next()
            .filter(|r| r.samples > 0)
            .map(|r| ObservedCadence {
                samples: r.samples.min(u64::from(u32::MAX)) as u32,
                median_gap: Duration::from_secs(u64::from(r.median_gap_secs)),
            }))
    }

    async fn ping(&self) -> Result<()> {
        self.client
            .query("SELECT 1")
            .execute()
            .await
            .context("clickhouse ping")?;
        Ok(())
    }

    async fn list_results(
        &self,
        org: OrgId,
        target_id: Uuid,
        range: ClampedRange,
        limit: usize,
        offset: usize,
        region: Option<&str>,
    ) -> Result<Vec<CheckResult>> {
        let limit = limit.min(10_000) as u64;
        let offset = offset as u64;
        let region_pred = region.map(|_| "AND region = ?").unwrap_or("");
        let mut q = self
            .client
            .query(&format!(
                "SELECT target_id, {RESULT_COLUMNS}, details FROM {TABLE} \
                 WHERE org_id = ? AND target_id = ? {region_pred} \
                 AND timestamp >= fromUnixTimestamp(?) \
                 AND timestamp < fromUnixTimestamp(?) \
                 ORDER BY timestamp DESC LIMIT ? OFFSET ?"
            ))
            .bind(org.0)
            .bind(target_id);
        if let Some(r) = region {
            q = q.bind(r);
        }
        let rows: Vec<OwnedResultRow> = q
            .bind(range.from.timestamp())
            .bind(range.to.timestamp())
            .bind(limit)
            .bind(offset)
            .fetch_all::<OwnedResultRow>()
            .await
            .context("clickhouse list_results")?;
        Ok(rows.into_iter().map(|r| row_to_result(r, org.0)).collect())
    }

    async fn list_results_by_region(
        &self,
        org: OrgId,
        target_id: Uuid,
        range: ClampedRange,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<(String, CheckResult)>> {
        let limit = limit.min(10_000) as u64;
        let offset = offset as u64;
        let rows: Vec<RegionResultRow> = self
            .client
            .query(&format!(
                "SELECT region, target_id, {RESULT_COLUMNS}, details FROM {TABLE} \
                 WHERE org_id = ? AND target_id = ? \
                 AND timestamp >= fromUnixTimestamp(?) \
                 AND timestamp < fromUnixTimestamp(?) \
                 ORDER BY timestamp DESC LIMIT ? OFFSET ?"
            ))
            .bind(org.0)
            .bind(target_id)
            .bind(range.from.timestamp())
            .bind(range.to.timestamp())
            .bind(limit)
            .bind(offset)
            .fetch_all::<RegionResultRow>()
            .await
            .context("clickhouse list_results_by_region")?;
        Ok(rows.into_iter().map(|r| r.split(org.0)).collect())
    }

    async fn latest_details_by_region(
        &self,
        org: OrgId,
        target_id: Uuid,
        range: ClampedRange,
        region: Option<&str>,
    ) -> Result<Vec<(String, CheckResult)>> {
        // The inner read touches only the key and the details null map, so the
        // full rows are read for one timestamp per region, not the window.
        let region_pred = region.map(|_| "AND region = ?").unwrap_or("");
        let mut q = self
            .client
            .query(&format!(
                "SELECT region, target_id, {RESULT_COLUMNS}, details FROM {TABLE} \
                 WHERE org_id = ? AND target_id = ? AND details IS NOT NULL \
                 AND timestamp >= fromUnixTimestamp(?) \
                 AND (region, timestamp) IN ( \
                     SELECT region, max(timestamp) FROM {TABLE} \
                     WHERE org_id = ? AND target_id = ? {region_pred} \
                     AND details IS NOT NULL \
                     AND timestamp >= fromUnixTimestamp(?) \
                     AND timestamp < fromUnixTimestamp(?) \
                     GROUP BY region) \
                 ORDER BY region LIMIT 1 BY region"
            ))
            .bind(org.0)
            .bind(target_id)
            .bind(range.from.timestamp())
            .bind(org.0)
            .bind(target_id);
        if let Some(r) = region {
            q = q.bind(r);
        }
        let rows: Vec<RegionResultRow> = q
            .bind(range.from.timestamp())
            .bind(range.to.timestamp())
            .fetch_all::<RegionResultRow>()
            .await
            .context("clickhouse latest_details_by_region")?;
        Ok(rows.into_iter().map(|r| r.split(org.0)).collect())
    }

    async fn list_failures_by_region(
        &self,
        org: OrgId,
        target_id: Uuid,
        range: ClampedRange,
        limit: usize,
        offset: usize,
        region: Option<&str>,
    ) -> Result<Vec<(String, CheckResult)>> {
        let limit = limit.min(10_000) as u64;
        let offset = offset as u64;
        let region_pred = region.map(|_| "AND region = ?").unwrap_or("");
        let mut q = self
            .client
            .query(&format!(
                "SELECT region, target_id, {RESULT_COLUMNS}, details FROM {TABLE} \
                 WHERE org_id = ? AND target_id = ? AND status != {up} {region_pred} \
                 AND timestamp >= fromUnixTimestamp(?) \
                 AND timestamp < fromUnixTimestamp(?) \
                 ORDER BY timestamp DESC LIMIT ? OFFSET ?",
                up = CheckStatus::Up.as_enum8(),
            ))
            .bind(org.0)
            .bind(target_id);
        if let Some(r) = region {
            q = q.bind(r);
        }
        let rows: Vec<RegionResultRow> = q
            .bind(range.from.timestamp())
            .bind(range.to.timestamp())
            .bind(limit)
            .bind(offset)
            .fetch_all::<RegionResultRow>()
            .await
            .context("clickhouse list_failures_by_region")?;
        Ok(rows.into_iter().map(|r| r.split(org.0)).collect())
    }

    async fn last_n_summary(
        &self,
        org: OrgId,
        range: TimeRange,
        region: Option<&str>,
    ) -> Result<(u64, u64, u32)> {
        let t = self
            .fleet_totals(org, range, region)
            .await
            .context("clickhouse last_n_summary counts")?;
        Ok((t.total, t.up, t.avg_ms()))
    }

    async fn failure_streaks(&self, org: OrgId, range: TimeRange, region: &str) -> Result<u64> {
        // Pull the entire range in one query ordered by (target_id, timestamp).
        // Coalesce per-target client-side; avoids one round-trip per target.
        let rows: Vec<IncidentRow> = self
            .client
            .query(&format!(
                "SELECT target_id, timestamp, status, error FROM {TABLE} \
                 WHERE org_id = ? AND region = ? \
                 AND timestamp >= fromUnixTimestamp(?) \
                 AND timestamp < fromUnixTimestamp(?) \
                 ORDER BY target_id ASC, timestamp ASC"
            ))
            .bind(org.0)
            .bind(region)
            .bind(range.from.timestamp())
            .bind(range.to.timestamp())
            .fetch_all::<IncidentRow>()
            .await
            .context("clickhouse failure_streaks")?;

        let mut streaks = 0u64;
        let mut group: Vec<IncidentRow> = Vec::new();
        let mut current_target: Option<Uuid> = None;
        for row in rows {
            match current_target {
                Some(t) if t == row.target_id => group.push(row),
                _ => {
                    if let Some(t) = current_target.take() {
                        streaks +=
                            coalesce_from_incident_rows(t, std::mem::take(&mut group)).len() as u64;
                    }
                    current_target = Some(row.target_id);
                    group.push(row);
                }
            }
        }
        if let Some(t) = current_target {
            streaks += coalesce_from_incident_rows(t, group).len() as u64;
        }
        Ok(streaks)
    }

    async fn sampled_targets(
        &self,
        org: OrgId,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<HashSet<Uuid>> {
        #[derive(Row, Deserialize)]
        struct Sampled {
            #[serde(with = "clickhouse::serde::uuid")]
            target_id: Uuid,
        }
        let src = self.source(org, from);
        let q = self
            .client
            .query(&format!(
                "SELECT DISTINCT target_id FROM {table} WHERE org_id = ? AND {window}",
                table = src.table(),
                window = src.window(),
            ))
            .bind(org.0);
        let rows: Vec<Sampled> = bind_window(q, from, to)
            .fetch_all::<Sampled>()
            .await
            .context("clickhouse sampled_targets")?;
        Ok(rows.into_iter().map(|r| r.target_id).collect())
    }

    async fn uptime(
        &self,
        org: OrgId,
        target_id: Uuid,
        range: ClampedRange,
        region: Option<&str>,
    ) -> Result<UptimeStats> {
        #[derive(Row, Deserialize)]
        struct CountsRow {
            total: u64,
            up: u64,
            down: u64,
            degraded: u64,
            error: u64,
        }

        let src = self.source(org, range.from);
        let region_pred = region.map(|_| "AND region = ?").unwrap_or("");
        let mut q = self
            .client
            .query(&format!(
                "SELECT {total} AS total, {up} AS up, {down} AS down, \
                        {degraded} AS degraded, {error} AS error \
                 FROM {table} \
                 WHERE org_id = ? AND target_id = ? {region_pred} AND {window}",
                total = src.total(),
                up = src.checks(CheckStatus::Up),
                down = src.checks(CheckStatus::Down),
                degraded = src.checks(CheckStatus::Degraded),
                error = src.checks(CheckStatus::Error),
                table = src.table(),
                window = src.window(),
            ))
            .bind(org.0)
            .bind(target_id);
        if let Some(r) = region {
            q = q.bind(r);
        }
        let row: CountsRow = fetch_single(bind_window(q, range.from, range.to))
            .await
            .context("clickhouse uptime")?;

        let mut stats = UptimeStats {
            total: row.total,
            up: row.up,
            down: row.down,
            degraded: row.degraded,
            error: row.error,
            ..Default::default()
        };
        stats.finish();
        Ok(stats)
    }

    async fn dashboard_rollup(
        &self,
        org: OrgId,
        range: TimeRange,
        region: Option<&str>,
    ) -> Result<Vec<DashboardMetrics>> {
        #[derive(Row, Deserialize)]
        struct RollupRow {
            #[serde(with = "clickhouse::serde::uuid")]
            target_id: Uuid,
            samples: u64,
            up: u64,
            avg_ms: f64,
            quantiles: Vec<f64>,
            last_status: i8,
            last_minute_ts: u32,
        }
        let src = self.source(org, range.from);
        let query = format!(
            "SELECT \
               target_id, \
               {total} AS samples, \
               {up} AS up, \
               {avg} AS avg_ms, \
               {quantiles} AS quantiles, \
               {last_status} AS last_status, \
               toUInt32(toStartOfMinute(max({time}))) AS last_minute_ts \
             FROM {table} \
             WHERE org_id = ? {region_pred} AND {window} \
             GROUP BY target_id",
            total = src.total(),
            up = src.checks(CheckStatus::Up),
            avg = src.avg_ms("duration"),
            quantiles = src.duration_quantiles("0.5, 0.95"),
            last_status = src.last_status(),
            time = src.time(),
            table = src.table(),
            window = src.window(),
            region_pred = region.map(|_| "AND region = ?").unwrap_or("")
        );
        let mut q = self.client.query(&query).bind(org.0);
        if let Some(r) = region {
            q = q.bind(r);
        }
        let rows: Vec<RollupRow> = bind_window(q, range.from, range.to)
            .fetch_all::<RollupRow>()
            .await
            .context("clickhouse dashboard_rollup")?;
        Ok(rows
            .into_iter()
            .map(|r| {
                let p50 = r.quantiles.first().copied().unwrap_or(0.0);
                let p95 = r.quantiles.get(1).copied().unwrap_or(0.0);
                let avg = if r.avg_ms.is_finite() { r.avg_ms } else { 0.0 };
                DashboardMetrics {
                    target_id: r.target_id,
                    samples: r.samples,
                    up: r.up,
                    avg_ms: avg.round().clamp(0.0, u32::MAX as f64) as u32,
                    p50_ms: p50.round().clamp(0.0, u32::MAX as f64) as u32,
                    p95_ms: p95.round().clamp(0.0, u32::MAX as f64) as u32,
                    last_status: CheckStatus::from_enum8(r.last_status).as_str().to_string(),
                    last_minute_ts: (r.last_minute_ts > 0).then_some(r.last_minute_ts as i64),
                }
            })
            .collect())
    }

    async fn last_check_times(
        &self,
        org: OrgId,
        target_ids: &[Uuid],
        since: DateTime<Utc>,
    ) -> Result<HashMap<Uuid, DateTime<Utc>>> {
        #[derive(Row, Deserialize)]
        struct LastRow {
            #[serde(with = "clickhouse::serde::uuid")]
            target_id: Uuid,
            last_ts: u32,
        }
        if target_ids.is_empty() {
            return Ok(HashMap::new());
        }
        // Typed ids, so the literal list is injection-safe; it hits the
        // (org_id, target_id, ...) sort key instead of reading the whole org.
        let id_list = target_ids
            .iter()
            .map(|t| format!("'{t}'"))
            .collect::<Vec<_>>()
            .join(",");
        let rows: Vec<LastRow> = self
            .client
            .query(&format!(
                "SELECT target_id, toUInt32(max(timestamp)) AS last_ts FROM {TABLE} \
                 WHERE org_id = ? AND target_id IN ({id_list}) \
                 AND timestamp >= fromUnixTimestamp(?) \
                 GROUP BY target_id"
            ))
            .bind(org.0)
            .bind(since.timestamp())
            .fetch_all::<LastRow>()
            .await
            .context("clickhouse last_check_times")?;
        Ok(rows
            .into_iter()
            .map(|r| (r.target_id, from_unix_secs(r.last_ts)))
            .collect())
    }

    async fn pass_marks(
        &self,
        targets: &[(OrgId, Uuid)],
        since: DateTime<Utc>,
    ) -> Result<HashMap<Uuid, PassMarks>> {
        #[derive(Row, Deserialize)]
        struct MarksRow {
            #[serde(with = "clickhouse::serde::uuid")]
            target_id: Uuid,
            last_checked: u32,
            last_passed: u32,
        }
        let mut marks = HashMap::with_capacity(targets.len());
        for chunk in targets.chunks(PAIRS_PER_QUERY) {
            let pair_list = pair_list(chunk);
            let rows: Vec<MarksRow> = self
                .client
                .query(&format!(
                    "SELECT target_id, toUInt32(max(hour)) AS last_checked, \
                            toUInt32(maxIf(hour, passed > 0)) AS last_passed \
                     FROM ( \
                        SELECT target_id, hour, \
                               countIfMerge(up_checks) + countIfMerge(degraded_checks) AS passed \
                        FROM check_results_1h \
                        WHERE (org_id, target_id) IN ({pair_list}) \
                          AND hour >= fromUnixTimestamp(?) \
                        GROUP BY target_id, hour \
                     ) \
                     GROUP BY target_id"
                ))
                .bind(since.timestamp())
                .fetch_all::<MarksRow>()
                .await
                .context("clickhouse pass_marks")?;
            marks.extend(rows.into_iter().map(|r| {
                let mark = PassMarks {
                    last_checked: from_unix_secs(r.last_checked),
                    last_passed: (r.last_passed > 0).then(|| from_unix_secs(r.last_passed)),
                };
                (r.target_id, mark)
            }));
        }
        Ok(marks)
    }

    async fn latest_status_by_region(
        &self,
        org: OrgId,
        range: TimeRange,
        target: Option<Uuid>,
    ) -> Result<Vec<RegionLatestStatus>> {
        // Unfiltered by region on purpose: the fold needs every region that
        // reported, and a single-region view reads raw status anyway.
        #[derive(Row, Deserialize)]
        struct StatusRow {
            #[serde(with = "clickhouse::serde::uuid")]
            target_id: Uuid,
            region: String,
            status: i8,
        }
        let src = self.source(org, range.from);
        let target_filter = if target.is_some() {
            "AND target_id = ?"
        } else {
            ""
        };
        let query = format!(
            "SELECT target_id, region, {last_status} AS status \
             FROM {table} \
             WHERE org_id = ? {target_filter} AND {window} \
             GROUP BY target_id, region",
            last_status = src.last_status(),
            table = src.table(),
            window = src.window(),
        );
        let mut q = self.client.query(&query).bind(org.0);
        if let Some(t) = target {
            q = q.bind(t);
        }
        let rows: Vec<StatusRow> = bind_window(q, range.from, range.to)
            .fetch_all::<StatusRow>()
            .await
            .context("clickhouse latest_status_by_region")?;
        Ok(rows
            .into_iter()
            .map(|r| RegionLatestStatus {
                target_id: r.target_id,
                region: r.region,
                status: CheckStatus::from_enum8(r.status),
            })
            .collect())
    }

    async fn dashboard_sparkline(
        &self,
        org: OrgId,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
        region: Option<&str>,
    ) -> Result<Vec<DashboardSparkBucket>> {
        #[derive(Row, Deserialize)]
        struct SparkRow {
            #[serde(with = "clickhouse::serde::uuid")]
            target_id: Uuid,
            bucket_ts: u32,
            avg_ms: f64,
            checks: u64,
            up: u64,
        }
        let src = self.source(org, from);
        let query = format!(
            "SELECT \
               target_id, \
               toUInt32(toStartOfMinute({time})) AS bucket_ts, \
               {avg} AS avg_ms, \
               {total} AS checks, \
               {up} AS up \
             FROM {table} \
             WHERE org_id = ? {region_pred} AND {window} \
             GROUP BY target_id, bucket_ts \
             ORDER BY target_id, bucket_ts",
            time = src.time(),
            avg = src.avg_ms("duration"),
            total = src.total(),
            up = src.checks(CheckStatus::Up),
            table = src.table(),
            window = src.window(),
            region_pred = region.map(|_| "AND region = ?").unwrap_or("")
        );
        let mut q = self.client.query(&query).bind(org.0);
        if let Some(r) = region {
            q = q.bind(r);
        }
        let rows: Vec<SparkRow> = bind_window(q, from, to)
            .fetch_all::<SparkRow>()
            .await
            .context("clickhouse dashboard_sparkline")?;
        Ok(rows
            .into_iter()
            .map(|r| DashboardSparkBucket {
                target_id: r.target_id,
                bucket_ts: r.bucket_ts as i64,
                avg_ms: r.avg_ms as f32,
                checks: r.checks,
                up: r.up,
            })
            .collect())
    }

    async fn latency_buckets(
        &self,
        org: OrgId,
        target_id: Uuid,
        range: ClampedRange,
        bucket_seconds: u32,
        region: Option<&str>,
    ) -> Result<Vec<LatencyBucket>> {
        #[derive(Row, Deserialize)]
        struct LatRow {
            bucket_ts: u32,
            // quantilesMerge(0.5, 0.95, 0.99) finalises to Array(Float64) of
            // length 3, in the level order requested.
            quantiles: Vec<f64>,
            avg: f64,
            dns: f64,
            connect: f64,
            tls: f64,
            ttfb: f64,
            samples: u64,
        }
        let bucket = rollup_bucket_secs(bucket_seconds);
        let src = self.source(org, range.from);
        let region_pred = region.map(|_| "AND region = ?").unwrap_or("");
        let query = format!(
            "SELECT {columns} \
             FROM {table} \
             WHERE org_id = ? AND target_id = ? {region_pred} AND {window} \
             GROUP BY bucket_ts \
             ORDER BY bucket_ts",
            columns = latency_columns(src, bucket),
            table = src.table(),
            window = src.window(),
        );
        let mut q = self.client.query(&query).bind(org.0).bind(target_id);
        if let Some(r) = region {
            q = q.bind(r);
        }
        let rows: Vec<LatRow> = bind_window(q, range.from, range.to)
            .fetch_all::<LatRow>()
            .await
            .context("clickhouse latency_buckets")?;
        // Phases are folded to 0 in SQL via ifNull, and every emitted bucket
        // has >= 1 sample, so each merged mean is a finite non-negative float.
        let ms = |v: f64| v.round().max(0.0) as u32;
        let buckets: Vec<LatencyBucket> = rows
            .into_iter()
            .map(|r| {
                let q = |i: usize| r.quantiles.get(i).copied().map(ms).unwrap_or(0);
                LatencyBucket {
                    t: i64::from(r.bucket_ts) * 1000,
                    p50: q(0),
                    p95: q(1),
                    p99: q(2),
                    avg: ms(r.avg),
                    dns: ms(r.dns),
                    connect: ms(r.connect),
                    tls: ms(r.tls),
                    ttfb: ms(r.ttfb),
                    samples: r.samples,
                }
            })
            .collect();
        tracing::debug!(
            org = %org.0,
            target = %target_id,
            bucket_seconds = bucket,
            buckets = buckets.len(),
            "latency_buckets served from rollup"
        );
        Ok(buckets)
    }

    async fn availability_buckets(
        &self,
        org: OrgId,
        target_id: Uuid,
        range: ClampedRange,
        bucket_seconds: u32,
        region: Option<&str>,
    ) -> Result<Vec<AvailabilityBucket>> {
        #[derive(Row, Deserialize)]
        struct AvailRow {
            bucket_ts: u32,
            total: u64,
            up: u64,
        }
        let src = self.source(org, range.from);
        let region_pred = region.map(|_| "AND region = ?").unwrap_or("");
        let query = format!(
            "SELECT {bucket} AS bucket_ts, {total} AS total, {up} AS up \
             FROM {table} \
             WHERE org_id = ? AND target_id = ? {region_pred} AND {window} \
             GROUP BY bucket_ts \
             ORDER BY bucket_ts",
            bucket = src.bucket(rollup_bucket_secs(bucket_seconds)),
            total = src.total(),
            up = src.checks(CheckStatus::Up),
            table = src.table(),
            window = src.window(),
        );
        let mut q = self.client.query(&query).bind(org.0).bind(target_id);
        if let Some(r) = region {
            q = q.bind(r);
        }
        let rows: Vec<AvailRow> = bind_window(q, range.from, range.to)
            .fetch_all::<AvailRow>()
            .await
            .context("clickhouse availability_buckets")?;
        Ok(rows
            .into_iter()
            .map(|r| AvailabilityBucket {
                bucket_ts: i64::from(r.bucket_ts),
                total: r.total,
                up: r.up,
            })
            .collect())
    }

    async fn region_breakdown(
        &self,
        org: OrgId,
        target_id: Uuid,
        range: TimeRange,
    ) -> Result<Vec<RegionRollup>> {
        #[derive(Row, Deserialize)]
        struct R {
            region: String,
            samples: u64,
            up: u64,
            quantiles: Vec<f64>,
            last_status: i8,
        }
        let src = self.source(org, range.from);
        let q = self
            .client
            .query(&format!(
                "SELECT region, {total} AS samples, {up} AS up, \
                        {quantiles} AS quantiles, {last_status} AS last_status \
                 FROM {table} \
                 WHERE org_id = ? AND target_id = ? AND {window} \
                 GROUP BY region ORDER BY region",
                total = src.total(),
                up = src.checks(CheckStatus::Up),
                quantiles = src.duration_quantiles("0.5, 0.95, 0.99"),
                last_status = src.last_status(),
                table = src.table(),
                window = src.window(),
            ))
            .bind(org.0)
            .bind(target_id);
        let rows: Vec<R> = bind_window(q, range.from, range.to)
            .fetch_all::<R>()
            .await
            .context("clickhouse region_breakdown")?;
        let ms = |v: f64| v.round().clamp(0.0, u32::MAX as f64) as u32;
        Ok(rows
            .into_iter()
            .map(|r| RegionRollup {
                region: r.region,
                samples: r.samples,
                up: r.up,
                p50_ms: ms(r.quantiles.first().copied().unwrap_or(0.0)),
                p95_ms: ms(r.quantiles.get(1).copied().unwrap_or(0.0)),
                p99_ms: ms(r.quantiles.get(2).copied().unwrap_or(0.0)),
                last_status: CheckStatus::from_enum8(r.last_status).as_str().to_string(),
            })
            .collect())
    }

    async fn flap_counts(
        &self,
        org: OrgId,
        target_id: Uuid,
        range: ClampedRange,
    ) -> Result<Vec<RegionFlaps>> {
        #[derive(Row, Deserialize)]
        struct F {
            region: String,
            failures: u64,
            transitions: u64,
        }
        let rows: Vec<F> = self
            .client
            .query(&format!(
                "SELECT region, countIf(status != {up}) AS failures, \
                   countIf(seq > 1 AND status != prev) AS transitions \
                 FROM ( \
                   SELECT region, status, \
                     any(status) OVER (PARTITION BY region ORDER BY timestamp \
                       ROWS BETWEEN 1 PRECEDING AND 1 PRECEDING) AS prev, \
                     row_number() OVER (PARTITION BY region ORDER BY timestamp) AS seq \
                   FROM {TABLE} \
                   WHERE org_id = ? AND target_id = ? \
                     AND timestamp >= fromUnixTimestamp(?) \
                     AND timestamp < fromUnixTimestamp(?) \
                 ) GROUP BY region ORDER BY region",
                up = CheckStatus::Up.as_enum8(),
            ))
            .bind(org.0)
            .bind(target_id)
            .bind(range.from.timestamp())
            .bind(range.to.timestamp())
            .fetch_all::<F>()
            .await
            .context("clickhouse flap_counts")?;
        Ok(rows
            .into_iter()
            .map(|r| RegionFlaps {
                region: r.region,
                failures: r.failures,
                transitions: r.transitions,
            })
            .collect())
    }

    async fn newest_by_region(
        &self,
        org: OrgId,
        target_id: Uuid,
        range: ClampedRange,
    ) -> Result<Vec<RegionNewest>> {
        #[derive(Row, Deserialize)]
        struct N {
            region: String,
            newest_at: u32,
            newest_status: i8,
            newest_error: String,
            diagnostic_kind: String,
            diagnostic_confidence: String,
            diagnostic_provider: String,
            passed: u8,
        }
        // `argMax` skips a NULL argument, so a bare nullable column would reach
        // past the newest row to an older one that had a value.
        let rows: Vec<N> = self
            .client
            .query(&format!(
                "SELECT region, \
                   toUInt32(max(timestamp)) AS newest_at, \
                   argMax(status, timestamp) AS newest_status, \
                   toString(argMax(ifNull(error, ''), timestamp)) AS newest_error, \
                   toString(argMax(ifNull(diagnostic_kind, ''), timestamp)) AS diagnostic_kind, \
                   toString(argMax(ifNull(diagnostic_confidence, ''), timestamp)) \
                     AS diagnostic_confidence, \
                   toString(argMax(ifNull(diagnostic_provider, ''), timestamp)) \
                     AS diagnostic_provider, \
                   countIf(status IN ({up}, {degraded})) > 0 AS passed \
                 FROM {TABLE} \
                 WHERE org_id = ? AND target_id = ? \
                   AND timestamp >= fromUnixTimestamp(?) \
                   AND timestamp < fromUnixTimestamp(?) \
                 GROUP BY region ORDER BY region",
                up = CheckStatus::Up.as_enum8(),
                degraded = CheckStatus::Degraded.as_enum8(),
            ))
            .bind(org.0)
            .bind(target_id)
            .bind(range.from.timestamp())
            .bind(range.to.timestamp())
            .fetch_all::<N>()
            .await
            .context("clickhouse newest_by_region")?;
        Ok(rows
            .into_iter()
            .map(|r| RegionNewest {
                region: r.region,
                at: from_unix_secs(r.newest_at),
                status: CheckStatus::from_enum8(r.newest_status),
                diagnostic: diagnostic_from(
                    Some(&r.diagnostic_kind),
                    Some(&r.diagnostic_confidence),
                    Some(&r.diagnostic_provider),
                    &[],
                ),
                error: (!r.newest_error.is_empty()).then_some(r.newest_error),
                passed: r.passed != 0,
            })
            .collect())
    }

    async fn failing_servers(
        &self,
        org: OrgId,
        target_id: Uuid,
        range: ClampedRange,
        limit: usize,
    ) -> Result<Vec<FailingServer>> {
        #[derive(Row, Deserialize)]
        struct S {
            ip: Ipv6Addr,
            regions: Vec<String>,
            checks: u64,
        }
        let rows: Vec<S> = self
            .client
            .query(&format!(
                "SELECT assumeNotNull(remote_ip) AS ip, \
                   arraySort(groupUniqArray(region)) AS regions, count() AS checks \
                 FROM {TABLE} \
                 WHERE org_id = ? AND target_id = ? AND status IN ({down}, {error}) \
                   AND remote_ip IS NOT NULL \
                   AND timestamp >= fromUnixTimestamp(?) \
                   AND timestamp < fromUnixTimestamp(?) \
                 GROUP BY ip ORDER BY checks DESC, ip LIMIT ?",
                down = CheckStatus::Down.as_enum8(),
                error = CheckStatus::Error.as_enum8(),
            ))
            .bind(org.0)
            .bind(target_id)
            .bind(range.from.timestamp())
            .bind(range.to.timestamp())
            .bind(limit as u64)
            .fetch_all::<S>()
            .await
            .context("clickhouse failing_servers")?;
        Ok(rows
            .into_iter()
            .map(|r| FailingServer {
                ip: r.ip.to_canonical(),
                regions: r.regions.into_iter().filter(|r| !r.is_empty()).collect(),
                checks: r.checks,
            })
            .collect())
    }

    async fn latency_buckets_by_region(
        &self,
        org: OrgId,
        target_id: Uuid,
        range: ClampedRange,
        bucket_seconds: u32,
    ) -> Result<Vec<RegionLatencySeries>> {
        #[derive(Row, Deserialize)]
        struct LatRow {
            region: String,
            bucket_ts: u32,
            quantiles: Vec<f64>,
            avg: f64,
            dns: f64,
            connect: f64,
            tls: f64,
            ttfb: f64,
            samples: u64,
        }
        let bucket = rollup_bucket_secs(bucket_seconds);
        let src = self.source(org, range.from);
        let query = format!(
            "SELECT region, {columns} \
             FROM {table} \
             WHERE org_id = ? AND target_id = ? AND {window} \
             GROUP BY region, bucket_ts \
             ORDER BY region, bucket_ts",
            columns = latency_columns(src, bucket),
            table = src.table(),
            window = src.window(),
        );
        let q = self.client.query(&query).bind(org.0).bind(target_id);
        let rows: Vec<LatRow> = bind_window(q, range.from, range.to)
            .fetch_all::<LatRow>()
            .await
            .context("clickhouse latency_buckets_by_region")?;
        let ms = |v: f64| v.round().max(0.0) as u32;
        let mut out: Vec<RegionLatencySeries> = Vec::new();
        for r in rows {
            let q = |i: usize| r.quantiles.get(i).copied().map(ms).unwrap_or(0);
            let bucket = LatencyBucket {
                t: i64::from(r.bucket_ts) * 1000,
                p50: q(0),
                p95: q(1),
                p99: q(2),
                avg: ms(r.avg),
                dns: ms(r.dns),
                connect: ms(r.connect),
                tls: ms(r.tls),
                ttfb: ms(r.ttfb),
                samples: r.samples,
            };
            match out.last_mut() {
                Some(s) if s.region == r.region => s.buckets.push(bucket),
                _ => out.push(RegionLatencySeries {
                    region: r.region,
                    label: String::new(),
                    buckets: vec![bucket],
                }),
            }
        }
        Ok(out)
    }

    async fn fleet_ribbon(
        &self,
        org: OrgId,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
        bucket_seconds: u32,
        region: Option<&str>,
    ) -> Result<Vec<FleetRibbonBucket>> {
        #[derive(Row, Deserialize)]
        struct RibbonRow {
            bucket_ts: u32,
            samples: u64,
            up: u64,
            // `Array(UUID)`: the crate's UUID adapter is single-value only, so
            // read each element in its RowBinary `(hi, lo)` form and rebuild.
            down_targets: Vec<(u64, u64)>,
        }
        let src = self.source(org, from);
        // Inner per-target pass so the outer can sum the fleet rate and also
        // collect which monitors dipped.
        let query = format!(
            "SELECT bucket_ts, sum(t_samples) AS samples, sum(t_up) AS up, \
                    groupUniqArrayIf(target_id, t_samples > t_up) AS down_targets \
             FROM ( \
               SELECT {bucket} AS bucket_ts, target_id, {total} AS t_samples, {up} AS t_up \
               FROM {table} \
               WHERE org_id = ? {region_pred} AND {window} \
               GROUP BY bucket_ts, target_id \
             ) \
             GROUP BY bucket_ts \
             ORDER BY bucket_ts",
            bucket = src.bucket(rollup_bucket_secs(bucket_seconds)),
            total = src.total(),
            up = src.checks(CheckStatus::Up),
            table = src.table(),
            window = src.window(),
            region_pred = region.map(|_| "AND region = ?").unwrap_or("")
        );
        let mut q = self.client.query(&query).bind(org.0);
        if let Some(r) = region {
            q = q.bind(r);
        }
        let rows: Vec<RibbonRow> = bind_window(q, from, to)
            .fetch_all::<RibbonRow>()
            .await
            .context("clickhouse fleet_ribbon")?;
        Ok(rows
            .into_iter()
            .map(|r| FleetRibbonBucket {
                bucket_ts: r.bucket_ts as i64,
                samples: r.samples,
                up: r.up,
                down_targets: r
                    .down_targets
                    .into_iter()
                    .map(|(hi, lo)| Uuid::from_u64_pair(hi, lo))
                    .collect(),
            })
            .collect())
    }

    async fn prior_period_summary(
        &self,
        org: OrgId,
        range: TimeRange,
        region: Option<&str>,
    ) -> Result<PriorPeriodSummary> {
        let span = range.to - range.from;
        let prior = TimeRange {
            from: range.from - span,
            to: range.from,
        };
        let t = self
            .fleet_totals(org, prior, region)
            .await
            .context("clickhouse prior_period_summary")?;
        Ok(PriorPeriodSummary {
            checks_total: t.total,
            checks_up: t.up,
            avg_ms: t.avg_ms(),
        })
    }
}

#[derive(Row, Deserialize)]
struct FleetTotals {
    total: u64,
    up: u64,
    avg_ms: f64,
}

impl FleetTotals {
    fn avg_ms(&self) -> u32 {
        if self.avg_ms.is_finite() {
            self.avg_ms.round().clamp(0.0, u32::MAX as f64) as u32
        } else {
            0
        }
    }
}

impl ClickhouseResultsStore {
    /// The current and prior KPI windows share one query shape; each still
    /// picks its source from its own start.
    async fn fleet_totals(
        &self,
        org: OrgId,
        range: TimeRange,
        region: Option<&str>,
    ) -> clickhouse::error::Result<FleetTotals> {
        let src = self.source(org, range.from);
        let region_pred = region.map(|_| "AND region = ?").unwrap_or("");
        let mut q = self
            .client
            .query(&format!(
                "SELECT {total} AS total, {up} AS up, {avg} AS avg_ms \
                 FROM {table} \
                 WHERE org_id = ? {region_pred} AND {window}",
                total = src.total(),
                up = src.checks(CheckStatus::Up),
                avg = src.avg_ms("duration"),
                table = src.table(),
                window = src.window(),
            ))
            .bind(org.0);
        if let Some(r) = region {
            q = q.bind(r);
        }
        fetch_single(bind_window(q, range.from, range.to)).await
    }
}

fn latency_columns(src: Source, bucket: u32) -> String {
    format!(
        "{bucket_ts} AS bucket_ts, \
         {quantiles} AS quantiles, \
         {avg} AS avg, \
         ifNull({dns}, 0) AS dns, \
         ifNull({connect}, 0) AS connect, \
         ifNull({tls}, 0) AS tls, \
         ifNull({ttfb}, 0) AS ttfb, \
         {total} AS samples",
        bucket_ts = src.bucket(bucket),
        quantiles = src.duration_quantiles("0.5, 0.95, 0.99"),
        avg = src.avg_ms("duration"),
        dns = src.avg_ms("dns"),
        connect = src.avg_ms("connect"),
        tls = src.avg_ms("tls"),
        ttfb = src.avg_ms("ttfb"),
        total = src.total(),
    )
}

#[cfg(test)]
mod tests {
    /// A page the background merge has not yet emptied, past its window.
    fn stored_run(evidence_expired: u8, stopped_step: Option<u16>) -> super::StoredFlowRunRow {
        super::StoredFlowRunRow {
            timestamp: 1_700_000_000,
            region: "eu-helsinki".into(),
            status: 2,
            duration_ms: 3_100,
            stopped_step,
            error: "step 2/2 assert_url: url does not contain \"/secure\"".into(),
            step_op: vec!["fill".into(), "assert_url".into()],
            step_outcome: vec![1, 2],
            step_ms: vec![40, 10_000],
            final_url: "https://app.example.com/login".into(),
            title: "Sign in".into(),
            text_snippet: "Your password is invalid!".into(),
            console_level: Vec::new(),
            console_text: Vec::new(),
            evidence_expired,
        }
    }

    #[test]
    fn a_page_past_its_window_is_dropped_even_before_the_merge_empties_it() {
        let view = super::FlowRunView::from(stored_run(1, Some(1)));
        assert!(view.evidence.is_none());
        assert!(view.evidence_expired);
        assert_eq!(view.steps.len(), 2, "the trace outlives the page");
    }

    #[test]
    fn a_page_inside_its_window_is_returned() {
        let view = super::FlowRunView::from(stored_run(0, Some(1)));
        let evidence = view.evidence.expect("still inside the window");
        assert_eq!(
            evidence.text_snippet.as_deref(),
            Some("Your password is invalid!")
        );
        assert!(!view.evidence_expired);
    }

    #[test]
    fn a_run_that_never_reached_a_step_lost_no_page() {
        assert!(!super::FlowRunView::from(stored_run(1, None)).evidence_expired);
    }
}
