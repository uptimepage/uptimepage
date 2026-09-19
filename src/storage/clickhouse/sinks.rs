use async_trait::async_trait;
use clickhouse::{Client, Row};
use metrics::counter;
use serde::Serialize;
use uuid::Uuid;

use crate::domain::agent_wire::{FlowRunRecord, StepOutcome};
use crate::domain::quota::RetentionDays;
use crate::domain::{CheckResult, HeartbeatPingRecord};
use crate::error::Result;
use crate::observability::metrics::names;
use crate::storage::org_ttl::OrgTtlDays;
use crate::storage::traits::{FlowRunSink, HeartbeatPingSink, ResultSink};

use super::{FLOW_TABLE, HEARTBEAT_PING_TABLE, TABLE, insert_backoff, to_unix_secs};

pub struct ClickhouseResultSink {
    client: Client,
    /// Region + agent id stamped on results from this control plane's own
    /// scheduler. Agent-submitted batches carry their own via [`write_batch_tagged`].
    region: String,
    agent_id: String,
    /// Per-org physical retention, resolved from the org's account plan at write time.
    org_ttl: OrgTtlDays,
}

impl ClickhouseResultSink {
    pub fn new(client: Client, region: String, agent_id: String, org_ttl: OrgTtlDays) -> Self {
        Self {
            client,
            region,
            agent_id,
            org_ttl,
        }
    }

    async fn write_once(
        &self,
        rows: &[CheckResultRow<'_>],
    ) -> std::result::Result<(), clickhouse::error::Error> {
        let mut insert = self.client.insert::<CheckResultRow>(TABLE).await?;
        for row in rows {
            insert.write(row).await?;
        }
        insert.end().await
    }

    async fn write_batch_inner(
        &self,
        results: &[CheckResult],
        region: &str,
        agent_id: &str,
    ) -> Result<()> {
        if results.is_empty() {
            return Ok(());
        }
        let ttls = self.org_ttl.days_for_each(results.iter().map(|r| r.org_id));
        let rows: Vec<CheckResultRow<'_>> = results
            .iter()
            .zip(ttls)
            .map(|(r, ttl)| CheckResultRow::from_result(r, region, agent_id, ttl.row))
            .collect();

        let backoff = insert_backoff();

        // Full re-send on each retry is intentional and safe ONLY because the
        // `check_results` table sets `non_replicated_deduplication_window` (see
        // 001_initial.sql): a commit-then-lost-ack retry re-sends the identical
        // block, which the server drops by hash instead of double-counting.
        // Don't checkpoint mid-batch here — a partial re-send changes the block
        // and defeats that dedup.
        let op = || async {
            self.write_once(&rows)
                .await
                .map_err(backoff::Error::transient)
        };

        if let Err(err) = backoff::future::retry(backoff, op).await {
            tracing::error!(
                ?err,
                count = rows.len(),
                "clickhouse insert failed after retries"
            );
            return Err(anyhow::anyhow!("clickhouse insert failed: {err}").into());
        }
        Ok(())
    }
}

#[async_trait]
impl ResultSink for ClickhouseResultSink {
    async fn write_batch(&self, results: &[CheckResult]) -> Result<()> {
        self.write_batch_inner(results, &self.region, &self.agent_id)
            .await
    }

    async fn write_batch_tagged(
        &self,
        results: &[CheckResult],
        region: &str,
        agent_id: &str,
    ) -> Result<()> {
        self.write_batch_inner(results, region, agent_id).await
    }
}

#[derive(Debug, Row, Serialize)]
struct CheckResultRow<'a> {
    #[serde(with = "clickhouse::serde::uuid")]
    org_id: Uuid,
    #[serde(with = "clickhouse::serde::uuid")]
    target_id: Uuid,
    // Sent explicitly, not via column DEFAULT: the matview groups on the
    // inserted block, so `region` must be in it.
    region: &'a str,
    timestamp: u32,
    agent_id: &'a str,
    status: i8,
    duration_ms: u32,
    dns_ms: Option<u16>,
    connect_ms: Option<u16>,
    tls_ms: Option<u16>,
    ttfb_ms: Option<u16>,
    response_code: Option<u16>,
    response_size: Option<u32>,
    error: Option<&'a str>,
    diagnostic_kind: Option<&'a str>,
    diagnostic_confidence: Option<&'a str>,
    diagnostic_provider: Option<&'a str>,
    diagnostic_evidence: Vec<&'a str>,
    ttl_days: u16,
}

impl<'a> CheckResultRow<'a> {
    fn from_result(r: &'a CheckResult, region: &'a str, agent_id: &'a str, ttl_days: u16) -> Self {
        Self {
            org_id: r.org_id,
            target_id: r.target_id,
            region,
            timestamp: to_unix_secs(r.timestamp),
            agent_id,
            status: r.status.as_enum8(),
            duration_ms: r.duration_ms,
            dns_ms: r.dns_ms,
            connect_ms: r.connect_ms,
            tls_ms: r.tls_ms,
            ttfb_ms: r.ttfb_ms,
            response_code: r.response_code,
            response_size: r.response_size,
            error: r.error.as_deref(),
            diagnostic_kind: r.diagnostic.as_ref().map(|d| d.kind.as_str()),
            diagnostic_confidence: r.diagnostic.as_ref().map(|d| d.confidence.as_str()),
            diagnostic_provider: r
                .diagnostic
                .as_ref()
                .and_then(|d| d.provider.map(|provider| provider.as_str())),
            diagnostic_evidence: r
                .diagnostic
                .as_ref()
                .map(|d| d.evidence.iter().map(|item| item.as_str()).collect())
                .unwrap_or_default(),
            ttl_days,
        }
    }
}

/// One row per browser-flow run. Unbatched: at the 300s interval floor these
/// arrive too rarely for a batcher to earn its keep.
pub struct ClickhouseFlowRunSink {
    client: Client,
    /// Region stamped on runs from this control plane's own scheduler.
    /// Agent-submitted runs carry their own via [`FlowRunSink::write_runs_tagged`].
    region: String,
    org_ttl: OrgTtlDays,
}

impl ClickhouseFlowRunSink {
    pub fn new(client: Client, region: String, org_ttl: OrgTtlDays) -> Self {
        Self {
            client,
            region,
            org_ttl,
        }
    }

    async fn write_once(
        &self,
        rows: &[FlowRunRow<'_>],
    ) -> std::result::Result<(), clickhouse::error::Error> {
        let mut insert = self.client.insert::<FlowRunRow>(FLOW_TABLE).await?;
        for row in rows {
            insert.write(row).await?;
        }
        insert.end().await
    }
}

#[async_trait]
impl FlowRunSink for ClickhouseFlowRunSink {
    async fn write_runs(&self, runs: &[FlowRunRecord]) {
        self.write_inner(runs, &self.region).await;
    }

    async fn write_runs_tagged(&self, runs: &[FlowRunRecord], region: &str) {
        self.write_inner(runs, region).await;
    }
}

impl ClickhouseFlowRunSink {
    async fn write_inner(&self, runs: &[FlowRunRecord], region: &str) {
        if runs.is_empty() {
            return;
        }
        let ttls = self.org_ttl.days_for_each(runs.iter().map(|r| r.org_id));
        let rows: Vec<FlowRunRow<'_>> = runs
            .iter()
            .zip(ttls)
            .map(|(r, ttl)| FlowRunRow::from_record(r, region, ttl))
            .collect();

        let backoff = insert_backoff();
        let op = || async {
            self.write_once(&rows)
                .await
                .map_err(backoff::Error::transient)
        };
        if let Err(err) = backoff::future::retry(backoff, op).await {
            // The verdict landed by its own path, so this costs history only.
            tracing::error!(
                ?err,
                count = rows.len(),
                "flow run insert failed after retries"
            );
            counter!(names::STORAGE_DROPPED, "reason" => "flow_run_write_failed")
                .increment(rows.len() as u64);
        }
    }
}

#[derive(Debug, Row, Serialize)]
struct FlowRunRow<'a> {
    #[serde(with = "clickhouse::serde::uuid")]
    org_id: Uuid,
    #[serde(with = "clickhouse::serde::uuid")]
    target_id: Uuid,
    region: &'a str,
    timestamp: u32,
    status: i8,
    duration_ms: u32,
    stopped_step: Option<u16>,
    error: &'a str,
    step_op: Vec<&'a str>,
    step_outcome: Vec<i8>,
    step_ms: Vec<u32>,
    final_url: &'a str,
    title: &'a str,
    text_snippet: &'a str,
    console_level: Vec<&'a str>,
    console_text: Vec<&'a str>,
    evidence_days: u16,
    ttl_days: u16,
}

impl<'a> FlowRunRow<'a> {
    fn from_record(r: &'a FlowRunRecord, region: &'a str, ttl: RetentionDays) -> Self {
        let ev = r.evidence.as_ref();
        let text = |o: &'a Option<String>| o.as_deref().unwrap_or_default();
        Self {
            org_id: r.org_id,
            target_id: r.target_id,
            region,
            timestamp: to_unix_secs(r.timestamp),
            status: r.status.as_enum8(),
            duration_ms: r.duration_ms,
            // The first entry that did not pass: the step it failed on, or the
            // one it never got to when the budget ran out. Either way it is the
            // step the error string names.
            stopped_step: r
                .steps
                .iter()
                .position(|s| s.outcome != StepOutcome::Passed)
                .map(|i| i.min(usize::from(u16::MAX)) as u16),
            error: r.error.as_deref().unwrap_or_default(),
            step_op: r.steps.iter().map(|s| s.op.as_str()).collect(),
            step_outcome: r.steps.iter().map(|s| s.outcome.as_enum8()).collect(),
            step_ms: r.steps.iter().map(|s| s.duration_ms).collect(),
            final_url: ev.map(|e| text(&e.final_url)).unwrap_or_default(),
            title: ev.map(|e| text(&e.title)).unwrap_or_default(),
            text_snippet: ev.map(|e| text(&e.text_snippet)).unwrap_or_default(),
            console_level: ev
                .map(|e| e.console.iter().map(|c| c.level.as_str()).collect())
                .unwrap_or_default(),
            console_text: ev
                .map(|e| e.console.iter().map(|c| c.text.as_str()).collect())
                .unwrap_or_default(),
            evidence_days: ttl.evidence,
            ttl_days: ttl.row,
        }
    }
}

/// Unbatched: the 60s period floor keeps these too rare for a batcher to earn
/// its keep.
pub struct ClickhouseHeartbeatPingSink {
    client: Client,
    org_ttl: OrgTtlDays,
}

impl ClickhouseHeartbeatPingSink {
    pub fn new(client: Client, org_ttl: OrgTtlDays) -> Self {
        Self { client, org_ttl }
    }
}

#[async_trait]
impl HeartbeatPingSink for ClickhouseHeartbeatPingSink {
    async fn write_ping(&self, ping: &HeartbeatPingRecord) {
        let row = HeartbeatPingRow::from_record(ping, self.org_ttl.days_for(ping.org_id));

        let backoff = insert_backoff();
        let op = || async {
            let mut insert = self
                .client
                .insert::<HeartbeatPingRow>(HEARTBEAT_PING_TABLE)
                .await
                .map_err(backoff::Error::transient)?;
            insert
                .write(&row)
                .await
                .map_err(backoff::Error::transient)?;
            insert.end().await.map_err(backoff::Error::transient)
        };
        if let Err(err) = backoff::future::retry(backoff, op).await {
            // The ping already moved the state the verdict reads, so this costs
            // history only.
            tracing::error!(?err, "heartbeat ping insert failed after retries");
            counter!(names::STORAGE_DROPPED, "reason" => "heartbeat_ping_write_failed")
                .increment(1);
        }
    }
}

#[derive(Debug, Row, Serialize)]
struct HeartbeatPingRow<'a> {
    #[serde(with = "clickhouse::serde::uuid")]
    org_id: Uuid,
    #[serde(with = "clickhouse::serde::uuid")]
    target_id: Uuid,
    received_at: u32,
    signal: i8,
    exit_code: Option<u8>,
    duration_ms: Option<u32>,
    body: &'a str,
    evidence_days: u16,
    ttl_days: u16,
}

impl<'a> HeartbeatPingRow<'a> {
    fn from_record(r: &'a HeartbeatPingRecord, ttl: RetentionDays) -> Self {
        Self {
            org_id: r.org_id,
            target_id: r.target_id,
            received_at: to_unix_secs(r.received_at),
            signal: r.signal.as_enum8(),
            exit_code: r.exit_code,
            duration_ms: r.duration_ms,
            body: &r.body,
            evidence_days: ttl.evidence,
            ttl_days: ttl.row,
        }
    }
}
