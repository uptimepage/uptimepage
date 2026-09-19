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
    CheckDiagnostic, CheckDiagnosticKind, CheckResult, CheckStatus, DiagnosticConfidence,
    DiagnosticEvidence, EdgeProvider, Incident, ObservedCadence, OrgId, coalesce_incidents,
};
use crate::error::Result;
use crate::storage::traits::{
    ClampedRange, FlowRunView, RegionFlaps, RegionLatestStatus, ResultsStore, TimeRange,
    UptimeStats, rollup_bucket_secs,
};

use super::{
    FLOW_TABLE, HEARTBEAT_PING_TABLE, MINUTE_WINDOW, TABLE, bind_minute_window, from_unix_secs,
    rollup_source, to_unix_secs,
};

/// Org-scoped read side of the ClickHouse results table. Holds no ambient
/// org; every query binds the `org` the caller resolved from the request, so
/// a `target_id` guessed from another tenant returns zero rows. `org_id` is
/// also the leading sort key — filtering on it is mandatory for performance,
/// not only isolation.
pub struct ClickhouseResultsStore {
    client: Client,
}

impl ClickhouseResultsStore {
    pub fn from_client(client: Client) -> Self {
        Self { client }
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
    error: Option<String>,
    diagnostic_kind: Option<String>,
    diagnostic_confidence: Option<String>,
    diagnostic_provider: Option<String>,
    diagnostic_evidence: Vec<String>,
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
    error: Option<String>,
    diagnostic_kind: Option<String>,
    diagnostic_confidence: Option<String>,
    diagnostic_provider: Option<String>,
    diagnostic_evidence: Vec<String>,
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
            error: self.error,
            diagnostic_kind: self.diagnostic_kind,
            diagnostic_confidence: self.diagnostic_confidence,
            diagnostic_provider: self.diagnostic_provider,
            diagnostic_evidence: self.diagnostic_evidence,
        };
        (self.region, row_to_result(inner, org_id))
    }
}

#[derive(Debug, Row, Deserialize)]
struct MultiResultRow {
    #[serde(with = "clickhouse::serde::uuid")]
    org_id: Uuid,
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
    error: Option<String>,
    diagnostic_kind: Option<String>,
    diagnostic_confidence: Option<String>,
    diagnostic_provider: Option<String>,
    diagnostic_evidence: Vec<String>,
}

impl MultiResultRow {
    fn split(self) -> (String, CheckResult) {
        let org_id = self.org_id;
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
            error: self.error,
            diagnostic_kind: self.diagnostic_kind,
            diagnostic_confidence: self.diagnostic_confidence,
            diagnostic_provider: self.diagnostic_provider,
            diagnostic_evidence: self.diagnostic_evidence,
        };
        (self.region, row_to_result(inner, org_id))
    }
}

fn row_to_result(row: OwnedResultRow, org_id: Uuid) -> CheckResult {
    let diagnostic = row
        .diagnostic_kind
        .as_deref()
        .and_then(CheckDiagnosticKind::parse)
        .map(|kind| CheckDiagnostic {
            kind,
            confidence: row
                .diagnostic_confidence
                .as_deref()
                .and_then(DiagnosticConfidence::parse)
                .unwrap_or(DiagnosticConfidence::Medium),
            provider: row
                .diagnostic_provider
                .as_deref()
                .and_then(EdgeProvider::parse),
            evidence: row
                .diagnostic_evidence
                .iter()
                .filter_map(|item| DiagnosticEvidence::parse(item))
                .collect(),
            // Derived so an old row cannot keep advice the product dropped.
            remediations: kind.remediations(),
        });
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
        diagnostic,
        error: row.error,
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
        let branch_cols =
            format!("{cols}, timestamp < now() - toIntervalDay(evidence_days) AS evidence_expired");
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
        // Not the rollup grain the other bucketed reads snap to — this one has
        // no rollup and no neighbouring chart to line up with. A floor is still
        // needed: a zero-second interval has no start to round to.
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
                "SELECT body FROM {HEARTBEAT_PING_TABLE} \
                 WHERE org_id = ? AND target_id = ? AND signal = 'fail' \
                   AND received_at = fromUnixTimestamp(?) LIMIT 1"
            ))
            .bind(org.0)
            .bind(target_id)
            .bind(to_unix_secs(at))
            .fetch_all::<String>()
            .await
            .context("clickhouse heartbeat failure output")?;
        // An expired `body` reads empty, same as a ping that carried none.
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
                "SELECT target_id, timestamp, status, duration_ms, dns_ms, connect_ms, tls_ms, \
                 ttfb_ms, response_code, response_size, error, diagnostic_kind, \
                 diagnostic_confidence, diagnostic_provider, diagnostic_evidence \
                 FROM {TABLE} \
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
                "SELECT region, target_id, timestamp, status, duration_ms, dns_ms, connect_ms, \
                 tls_ms, ttfb_ms, response_code, response_size, error, diagnostic_kind, \
                 diagnostic_confidence, diagnostic_provider, diagnostic_evidence \
                 FROM {TABLE} \
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
                "SELECT region, target_id, timestamp, status, duration_ms, dns_ms, connect_ms, \
                 tls_ms, ttfb_ms, response_code, response_size, error, diagnostic_kind, \
                 diagnostic_confidence, diagnostic_provider, diagnostic_evidence \
                 FROM {TABLE} \
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

    async fn recent_results_for_targets(
        &self,
        targets: &[(OrgId, Uuid)],
        range: ClampedRange,
        per_target_limit: usize,
    ) -> Result<Vec<(String, CheckResult)>> {
        if targets.is_empty() {
            return Ok(Vec::new());
        }
        let limit = per_target_limit.min(10_000) as u64;
        // Pair IN over typed, unique ids (injection-safe): hits the
        // (org_id, target_id, ...) sort-key prefix so CH prunes by org instead
        // of scanning every tenant's granules.
        let pair_list = targets
            .iter()
            .map(|(o, t)| format!("('{}','{}')", o.0, t))
            .collect::<Vec<_>>()
            .join(",");
        let rows: Vec<MultiResultRow> = self
            .client
            .query(&format!(
                "SELECT org_id, region, target_id, timestamp, status, duration_ms, dns_ms, \
                 connect_ms, tls_ms, ttfb_ms, response_code, response_size, error, \
                 diagnostic_kind, diagnostic_confidence, diagnostic_provider, \
                 diagnostic_evidence FROM {TABLE} \
                 WHERE (org_id, target_id) IN ({pair_list}) \
                 AND timestamp >= fromUnixTimestamp(?) \
                 AND timestamp < fromUnixTimestamp(?) \
                 ORDER BY target_id, region, timestamp DESC \
                 LIMIT {limit} BY target_id, region"
            ))
            .bind(range.from.timestamp())
            .bind(range.to.timestamp())
            .fetch_all::<MultiResultRow>()
            .await
            .context("clickhouse recent_results_for_targets")?;
        Ok(rows.into_iter().map(MultiResultRow::split).collect())
    }

    async fn last_n_summary(
        &self,
        org: OrgId,
        range: TimeRange,
        region: Option<&str>,
    ) -> Result<(u64, u64, u32, u64)> {
        #[derive(Row, Deserialize)]
        struct Counts {
            total: u64,
            up: u64,
            avg_ms: f64,
        }
        let (table, tcol) = rollup_source(range.from);
        let window = format!("{tcol} >= fromUnixTimestamp(?) AND {tcol} < fromUnixTimestamp(?)");
        let region_pred = region.map(|_| "AND region = ?").unwrap_or("");
        let mut cq = self
            .client
            .query(&format!(
                "SELECT countMerge(total_checks) AS total, \
                        countIfMerge(up_checks) AS up, \
                        avgMerge(avg_duration_ms) AS avg_ms \
                 FROM {table} \
                 WHERE org_id = ? {region_pred} AND {window}"
            ))
            .bind(org.0);
        if let Some(r) = region {
            cq = cq.bind(r);
        }
        let counts: Counts = bind_minute_window(cq, range.from, range.to)
            .fetch_one::<Counts>()
            .await
            .context("clickhouse last_n_summary counts")?;

        // Pull the entire range in one query ordered by (target_id, timestamp).
        // Coalesce per-target client-side; avoids one round-trip per target.
        let mut rq = self
            .client
            .query(&format!(
                "SELECT target_id, timestamp, status, error FROM {TABLE} \
                 WHERE org_id = ? {region_pred} \
                 AND timestamp >= fromUnixTimestamp(?) \
                 AND timestamp < fromUnixTimestamp(?) \
                 ORDER BY target_id ASC, timestamp ASC"
            ))
            .bind(org.0);
        if let Some(r) = region {
            rq = rq.bind(r);
        }
        let rows: Vec<IncidentRow> = rq
            .bind(range.from.timestamp())
            .bind(range.to.timestamp())
            .fetch_all::<IncidentRow>()
            .await
            .context("clickhouse last_n_summary rows")?;

        let mut incidents = 0u64;
        let mut group: Vec<IncidentRow> = Vec::new();
        let mut current_target: Option<Uuid> = None;
        for row in rows {
            match current_target {
                Some(t) if t == row.target_id => group.push(row),
                _ => {
                    if let Some(t) = current_target.take() {
                        incidents +=
                            coalesce_from_incident_rows(t, std::mem::take(&mut group)).len() as u64;
                    }
                    current_target = Some(row.target_id);
                    group.push(row);
                }
            }
        }
        if let Some(t) = current_target {
            incidents += coalesce_from_incident_rows(t, group).len() as u64;
        }
        let avg_ms = if counts.avg_ms.is_finite() {
            counts.avg_ms.round().clamp(0.0, u32::MAX as f64) as u32
        } else {
            0
        };
        Ok((counts.total, counts.up, avg_ms, incidents))
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

        let (table, tcol) = rollup_source(range.from);
        let window = format!("{tcol} >= fromUnixTimestamp(?) AND {tcol} < fromUnixTimestamp(?)");
        let region_pred = region.map(|_| "AND region = ?").unwrap_or("");
        let mut q = self
            .client
            .query(&format!(
                "SELECT \
                   countMerge(total_checks) AS total, \
                   countIfMerge(up_checks) AS up, \
                   countIfMerge(down_checks) AS down, \
                   countIfMerge(degraded_checks) AS degraded, \
                   countIfMerge(error_checks) AS error \
                 FROM {table} \
                 WHERE org_id = ? AND target_id = ? {region_pred} AND {window}"
            ))
            .bind(org.0)
            .bind(target_id);
        if let Some(r) = region {
            q = q.bind(r);
        }
        let row: CountsRow = bind_minute_window(q, range.from, range.to)
            .fetch_one::<CountsRow>()
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
        // Merges from the per-minute matview, not raw `check_results`.
        // `minute` is `DateTime` (UInt32 s), so the bind is u32-seconds.
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
        let (table, tcol) = rollup_source(range.from);
        let query = format!(
            "SELECT \
               target_id, \
               countMerge(total_checks) AS samples, \
               countIfMerge(up_checks) AS up, \
               avgMerge(avg_duration_ms) AS avg_ms, \
               quantilesMerge(0.5, 0.95)(duration_quantiles) AS quantiles, \
               argMaxMerge(last_status_state) AS last_status, \
               toUInt32(max({tcol})) AS last_minute_ts \
             FROM {table} \
             WHERE org_id = ? {region_pred} \
               AND {tcol} >= fromUnixTimestamp(?) AND {tcol} < fromUnixTimestamp(?) \
             GROUP BY target_id",
            region_pred = region.map(|_| "AND region = ?").unwrap_or("")
        );
        let mut q = self.client.query(&query).bind(org.0);
        if let Some(r) = region {
            q = q.bind(r);
        }
        let rows: Vec<RollupRow> = bind_minute_window(q, range.from, range.to)
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

    async fn latest_status_by_region(
        &self,
        org: OrgId,
        range: TimeRange,
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
        let (table, tcol) = rollup_source(range.from);
        let query = format!(
            "SELECT \
               target_id, \
               region, \
               argMaxMerge(last_status_state) AS status \
             FROM {table} \
             WHERE org_id = ? \
               AND {tcol} >= fromUnixTimestamp(?) AND {tcol} < fromUnixTimestamp(?) \
             GROUP BY target_id, region"
        );
        let rows: Vec<StatusRow> =
            bind_minute_window(self.client.query(&query).bind(org.0), range.from, range.to)
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
        // Reads the per-minute rollup so cost stays O(buckets), not O(raw). The
        // `avgState` column needs its `avgMerge` finaliser in `SELECT`.
        #[derive(Row, Deserialize)]
        struct SparkRow {
            #[serde(with = "clickhouse::serde::uuid")]
            target_id: Uuid,
            bucket_ts: u32,
            avg_ms: f64,
            checks: u64,
            up: u64,
        }
        let query = format!(
            "SELECT \
               target_id, \
               toUInt32(minute) AS bucket_ts, \
               avgMerge(avg_duration_ms) AS avg_ms, \
               countMerge(total_checks) AS checks, \
               countIfMerge(up_checks) AS up \
             FROM check_results_1m \
             WHERE org_id = ? {region_pred} AND {MINUTE_WINDOW} \
             GROUP BY target_id, minute \
             ORDER BY target_id, minute",
            region_pred = region.map(|_| "AND region = ?").unwrap_or("")
        );
        let mut q = self.client.query(&query).bind(org.0);
        if let Some(r) = region {
            q = q.bind(r);
        }
        let rows: Vec<SparkRow> = bind_minute_window(q, from, to)
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
        // `minute`/`hour` are both DateTime seconds, so `bind_minute_window`
        // binds either.
        let (table, tcol) = rollup_source(range.from);
        let window = format!("{tcol} >= fromUnixTimestamp(?) AND {tcol} < fromUnixTimestamp(?)");
        let region_pred = region.map(|_| "AND region = ?").unwrap_or("");
        let query = format!(
            "SELECT \
               toUInt32(toStartOfInterval({tcol}, INTERVAL {bucket} SECOND)) AS bucket_ts, \
               quantilesMerge(0.5, 0.95, 0.99)(duration_quantiles) AS quantiles, \
               avgMerge(avg_duration_ms) AS avg, \
               ifNull(avgMerge(avg_dns_ms), 0) AS dns, \
               ifNull(avgMerge(avg_connect_ms), 0) AS connect, \
               ifNull(avgMerge(avg_tls_ms), 0) AS tls, \
               ifNull(avgMerge(avg_ttfb_ms), 0) AS ttfb, \
               countMerge(total_checks) AS samples \
             FROM {table} \
             WHERE org_id = ? AND target_id = ? {region_pred} AND {window} \
             GROUP BY bucket_ts \
             ORDER BY bucket_ts"
        );
        let mut q = self.client.query(&query).bind(org.0).bind(target_id);
        if let Some(r) = region {
            q = q.bind(r);
        }
        let rows: Vec<LatRow> = bind_minute_window(q, range.from, range.to)
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
        let bucket = rollup_bucket_secs(bucket_seconds);
        let (table, tcol) = rollup_source(range.from);
        let window = format!("{tcol} >= fromUnixTimestamp(?) AND {tcol} < fromUnixTimestamp(?)");
        let region_pred = region.map(|_| "AND region = ?").unwrap_or("");
        let query = format!(
            "SELECT \
               toUInt32(toStartOfInterval({tcol}, INTERVAL {bucket} SECOND)) AS bucket_ts, \
               countMerge(total_checks) AS total, \
               countIfMerge(up_checks) AS up \
             FROM {table} \
             WHERE org_id = ? AND target_id = ? {region_pred} AND {window} \
             GROUP BY bucket_ts \
             ORDER BY bucket_ts"
        );
        let mut q = self.client.query(&query).bind(org.0).bind(target_id);
        if let Some(r) = region {
            q = q.bind(r);
        }
        let rows: Vec<AvailRow> = bind_minute_window(q, range.from, range.to)
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
        let q = self
            .client
            .query(&format!(
                "SELECT region, \
                   countMerge(total_checks) AS samples, \
                   countIfMerge(up_checks) AS up, \
                   quantilesMerge(0.5, 0.95, 0.99)(duration_quantiles) AS quantiles, \
                   argMaxMerge(last_status_state) AS last_status \
                 FROM check_results_1m \
                 WHERE org_id = ? AND target_id = ? AND {MINUTE_WINDOW} \
                 GROUP BY region ORDER BY region"
            ))
            .bind(org.0)
            .bind(target_id);
        let rows: Vec<R> = bind_minute_window(q, range.from, range.to)
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
        let (table, tcol) = rollup_source(range.from);
        let window = format!("{tcol} >= fromUnixTimestamp(?) AND {tcol} < fromUnixTimestamp(?)");
        let query = format!(
            "SELECT region, \
               toUInt32(toStartOfInterval({tcol}, INTERVAL {bucket} SECOND)) AS bucket_ts, \
               quantilesMerge(0.5, 0.95, 0.99)(duration_quantiles) AS quantiles, \
               avgMerge(avg_duration_ms) AS avg, \
               ifNull(avgMerge(avg_dns_ms), 0) AS dns, \
               ifNull(avgMerge(avg_connect_ms), 0) AS connect, \
               ifNull(avgMerge(avg_tls_ms), 0) AS tls, \
               ifNull(avgMerge(avg_ttfb_ms), 0) AS ttfb, \
               countMerge(total_checks) AS samples \
             FROM {table} \
             WHERE org_id = ? AND target_id = ? AND {window} \
             GROUP BY region, bucket_ts \
             ORDER BY region, bucket_ts"
        );
        let q = self.client.query(&query).bind(org.0).bind(target_id);
        let rows: Vec<LatRow> = bind_minute_window(q, range.from, range.to)
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
        let bucket = rollup_bucket_secs(bucket_seconds);
        // Inner per-target pass so the outer can sum the fleet rate and also
        // collect which monitors dipped. INTERVAL is a keyword position —
        // inlined so `bind()` stays value-only.
        let query = format!(
            "SELECT bucket_ts, sum(t_samples) AS samples, sum(t_up) AS up, \
                    groupUniqArrayIf(target_id, t_samples > t_up) AS down_targets \
             FROM ( \
               SELECT \
                 toUInt32(toStartOfInterval(minute, INTERVAL {bucket} SECOND)) AS bucket_ts, \
                 target_id, \
                 countMerge(total_checks) AS t_samples, \
                 countIfMerge(up_checks) AS t_up \
               FROM check_results_1m \
               WHERE org_id = ? {region_pred} AND {MINUTE_WINDOW} \
               GROUP BY bucket_ts, target_id \
             ) \
             GROUP BY bucket_ts \
             ORDER BY bucket_ts",
            region_pred = region.map(|_| "AND region = ?").unwrap_or("")
        );
        let mut q = self.client.query(&query).bind(org.0);
        if let Some(r) = region {
            q = q.bind(r);
        }
        let rows: Vec<RibbonRow> = bind_minute_window(q, from, to)
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
        // Same rollup source as `last_n_summary`: the dashboard compares
        // current (from `last_n_summary`) against prior here, so a source
        // mismatch would produce phantom deltas during ingest lag.
        #[derive(Row, Deserialize)]
        struct PriorRow {
            samples: u64,
            up: u64,
            avg_ms: f64,
        }
        let span = range.to - range.from;
        let prior_to = range.from;
        let prior_from = prior_to - span;
        let (table, tcol) = rollup_source(prior_from);
        let window = format!("{tcol} >= fromUnixTimestamp(?) AND {tcol} < fromUnixTimestamp(?)");
        let region_pred = region.map(|_| "AND region = ?").unwrap_or("");
        let mut q = self
            .client
            .query(&format!(
                "SELECT \
                   countMerge(total_checks) AS samples, \
                   countIfMerge(up_checks) AS up, \
                   avgMerge(avg_duration_ms) AS avg_ms \
                 FROM {table} \
                 WHERE org_id = ? {region_pred} AND {window}"
            ))
            .bind(org.0);
        if let Some(r) = region {
            q = q.bind(r);
        }
        let row: Option<PriorRow> = bind_minute_window(q, prior_from, prior_to)
            .fetch_optional::<PriorRow>()
            .await
            .context("clickhouse prior_period_summary")?;
        let r = row.unwrap_or(PriorRow {
            samples: 0,
            up: 0,
            avg_ms: 0.0,
        });
        let avg_ms = if r.avg_ms.is_finite() {
            r.avg_ms.round().clamp(0.0, u32::MAX as f64) as u32
        } else {
            0
        };
        Ok(PriorPeriodSummary {
            checks_total: r.samples,
            checks_up: r.up,
            avg_ms,
        })
    }
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
