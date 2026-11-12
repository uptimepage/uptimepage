use std::net::{IpAddr, Ipv6Addr};

use async_trait::async_trait;
use clickhouse::{Client, Row};
use metrics::counter;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::domain::agent_wire::{FlowRunRecord, StepOutcome};
use crate::domain::quota::RetentionDays;
use crate::domain::{CheckDetails, CheckResult, HeartbeatPingRecord, OrgId};
use crate::error::Result;
use crate::metric_names;
use crate::pagination::AlertEventCursor;
use crate::storage::alert_delivery::{Delivery, Status};
use crate::storage::org_ttl::OrgTtlDays;
use crate::storage::traits::{
    AlertEventCounts, AlertEventHour, AlertEventLog, AlertEventSink, AlertEventView, FlowRunSink,
    HeartbeatPingSink, ResultSink,
};

use super::{
    ALERT_EVENT_TABLE, FLOW_TABLE, HEARTBEAT_PING_TABLE, TABLE, insert_backoff, past_evidence,
    to_unix_secs,
};

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
        // `check_results` table and its rollups set
        // `non_replicated_deduplication_window` (see 001_initial.sql and
        // `dedup_retries_in_rollups`): a commit-then-lost-ack retry re-sends the
        // identical block, which the server drops by hash instead of
        // double-counting.
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
    remote_ip: Option<Ipv6Addr>,
    tried_ips: Vec<Ipv6Addr>,
    hop_url: Option<&'a str>,
    dns_resolver: Option<Ipv6Addr>,
    dns_answer: Vec<Ipv6Addr>,
    dns_ttl: Option<u32>,
    error: Option<&'a str>,
    details: Option<String>,
    diagnostic_kind: Option<&'a str>,
    diagnostic_confidence: Option<&'a str>,
    diagnostic_provider: Option<&'a str>,
    diagnostic_evidence: Vec<&'a str>,
    ttl_days: u16,
}

impl<'a> CheckResultRow<'a> {
    fn from_result(r: &'a CheckResult, region: &'a str, agent_id: &'a str, ttl_days: u16) -> Self {
        let network = r.network.as_ref();
        let dns = network.and_then(|n| n.dns.as_ref());
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
            remote_ip: network.and_then(|n| n.remote_ip).map(mapped),
            tried_ips: network
                .map(|n| n.tried_ips.iter().copied().map(mapped).collect())
                .unwrap_or_default(),
            hop_url: network.and_then(|n| n.hop_url.as_deref()),
            dns_resolver: dns.map(|d| mapped(d.resolver)),
            dns_answer: dns
                .map(|d| d.addresses.iter().copied().map(mapped).collect())
                .unwrap_or_default(),
            dns_ttl: dns.map(|d| d.ttl_secs),
            error: r.error.as_deref(),
            details: r.details.as_ref().map(CheckDetails::stored_json),
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

fn mapped(ip: IpAddr) -> Ipv6Addr {
    match ip {
        IpAddr::V4(v4) => v4.to_ipv6_mapped(),
        IpAddr::V6(v6) => v6,
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
            counter!(metric_names::STORAGE_DROPPED, "reason" => "flow_run_write_failed")
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
            counter!(metric_names::STORAGE_DROPPED, "reason" => "heartbeat_ping_write_failed")
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

/// The delivery log's outcome column, as its `Enum8` stores it. A deleted
/// source's events are not logged, so `Gone` has no value.
const OUTCOMES: [(Status, i8); 9] = [
    (Status::Opened, 1),
    (Status::Repeat, 2),
    (Status::Resolved, 3),
    (Status::Ignored, 4),
    (Status::Paused, 5),
    (Status::Held, 6),
    (Status::Capped, 7),
    (Status::Unparsed, 8),
    (Status::Unavailable, 9),
];

/// One insert per request, unbatched: a request is already a batch. Reads
/// the log back too.
pub struct ClickhouseAlertEventSink {
    client: Client,
    org_ttl: OrgTtlDays,
}

impl ClickhouseAlertEventSink {
    pub fn new(client: Client, org_ttl: OrgTtlDays) -> Self {
        Self { client, org_ttl }
    }
}

#[async_trait]
impl AlertEventSink for ClickhouseAlertEventSink {
    async fn write_deliveries(&self, rows: &[Delivery]) {
        let Some(first) = rows.first() else {
            return;
        };
        let ttl = self.org_ttl.days_for(first.org_id);
        let rows: Vec<AlertEventRow<'_>> = rows
            .iter()
            .filter_map(|r| AlertEventRow::from_delivery(r, ttl))
            .collect();
        let backoff = insert_backoff();
        let op = || async {
            let mut insert = self
                .client
                .insert::<AlertEventRow>(ALERT_EVENT_TABLE)
                .await
                .map_err(backoff::Error::transient)?;
            for row in &rows {
                insert.write(row).await.map_err(backoff::Error::transient)?;
            }
            insert.end().await.map_err(backoff::Error::transient)
        };
        if let Err(err) = backoff::future::retry(backoff, op).await {
            tracing::error!(?err, "alert event insert failed after retries");
            counter!(metric_names::STORAGE_DROPPED, "reason" => "alert_event_write_failed")
                .increment(rows.len() as u64);
        }
    }
}

#[async_trait]
impl AlertEventLog for ClickhouseAlertEventSink {
    async fn recent(
        &self,
        org: OrgId,
        source: Uuid,
        outcomes: &[Status],
        after: Option<&AlertEventCursor>,
        limit: usize,
    ) -> Result<Vec<AlertEventView>> {
        let outcome_clause = if outcomes.is_empty() {
            String::new()
        } else {
            format!("AND toInt8(outcome) IN ({}) ", outcome_codes(outcomes))
        };
        let after_clause = if after.is_some() {
            "AND (received_at < fromUnixTimestamp64Milli(?) \
                  OR (received_at = fromUnixTimestamp64Milli(?) \
                      AND (dedup_key, event_id) > (?, toUUID(?)))) "
        } else {
            ""
        };
        // TTL drops rows and bodies only when a merge runs, so both windows
        // are applied here too: a row past its retention reads as gone, a
        // body past its evidence window as empty.
        let mut q = self
            .client
            .query(&format!(
                "SELECT event_id, received_at, toInt8(outcome) AS outcome, dedup_key, incident_id, \
                     note, if({expired}, '', body) AS body \
                 FROM {ALERT_EVENT_TABLE} \
                 WHERE org_id = ? AND source_id = ? \
                   AND received_at >= now() - toIntervalDay(ttl_days) {outcome_clause}{after_clause}\
                 ORDER BY received_at DESC, dedup_key ASC, event_id ASC LIMIT {limit}",
                expired = past_evidence("toDateTime(received_at)"),
            ))
            .bind(org.0)
            .bind(source);
        if let Some(after) = after {
            let at = after.received_at.timestamp_millis();
            q = q
                .bind(at)
                .bind(at)
                .bind(after.dedup_key.as_str())
                .bind(after.event_id);
        }
        let rows = q
            .fetch_all::<AlertEventReadRow>()
            .await
            .map_err(|e| anyhow::anyhow!("clickhouse alert events: {e}"))?;
        // An outcome this build does not know is a schema drift, not a row to
        // drop: dropping it would shorten the page and end the log early.
        rows.into_iter()
            .map(|r| {
                let outcome = OUTCOMES
                    .iter()
                    .find(|(_, code)| *code == r.outcome)
                    .ok_or_else(|| anyhow::anyhow!("alert event outcome {} unknown", r.outcome))?
                    .0;
                Ok(AlertEventView {
                    id: r.event_id,
                    received_at: r.received_at,
                    outcome,
                    dedup_key: r.dedup_key,
                    incident_id: r.incident_id,
                    note: r.note,
                    body: r.body,
                })
            })
            .collect()
    }

    async fn hourly(
        &self,
        org: OrgId,
        source: Option<Uuid>,
        since: chrono::DateTime<chrono::Utc>,
    ) -> Result<Vec<AlertEventHour>> {
        #[derive(Row, Deserialize)]
        struct HourRow {
            #[serde(with = "clickhouse::serde::uuid")]
            source_id: Uuid,
            hour: u32,
            events: u64,
            not_applied: u64,
        }
        let source_clause = if source.is_some() {
            "AND source_id = ? "
        } else {
            ""
        };
        let mut q = self
            .client
            .query(&format!(
                "SELECT source_id, toUnixTimestamp(toStartOfHour(received_at)) AS hour, \
                     count() AS events, \
                     countIf(toInt8(outcome) IN ({})) AS not_applied \
                 FROM {ALERT_EVENT_TABLE} \
                 WHERE org_id = ? {source_clause}AND received_at >= fromUnixTimestamp64Milli(?) \
                 GROUP BY source_id, hour",
                outcome_codes(&Status::NOT_APPLIED),
            ))
            .bind(org.0);
        if let Some(source) = source {
            q = q.bind(source);
        }
        let rows = q
            .bind(since.timestamp_millis())
            .fetch_all::<HourRow>()
            .await
            .map_err(|e| anyhow::anyhow!("clickhouse alert event hours: {e}"))?;
        Ok(rows
            .into_iter()
            .map(|r| AlertEventHour {
                source: r.source_id,
                hour: chrono::DateTime::from_timestamp(i64::from(r.hour), 0).unwrap_or_default(),
                counts: AlertEventCounts {
                    events: r.events,
                    not_applied: r.not_applied,
                },
            })
            .collect())
    }
}

/// The stored codes of `outcomes`, for an `IN (…)` list. Integers from a
/// fixed table, so nothing caller-written reaches the query.
fn outcome_codes(outcomes: &[Status]) -> String {
    OUTCOMES
        .iter()
        .filter(|(status, _)| outcomes.contains(status))
        .map(|(_, code)| code.to_string())
        .collect::<Vec<_>>()
        .join(",")
}

#[derive(Debug, Row, Deserialize)]
struct AlertEventReadRow {
    #[serde(with = "clickhouse::serde::uuid")]
    event_id: Uuid,
    #[serde(with = "clickhouse::serde::chrono::datetime64::millis")]
    received_at: chrono::DateTime<chrono::Utc>,
    outcome: i8,
    dedup_key: String,
    #[serde(with = "clickhouse::serde::uuid::option")]
    incident_id: Option<Uuid>,
    note: String,
    body: String,
}

#[derive(Debug, Row, Serialize)]
struct AlertEventRow<'a> {
    #[serde(with = "clickhouse::serde::uuid")]
    org_id: Uuid,
    #[serde(with = "clickhouse::serde::uuid")]
    source_id: Uuid,
    #[serde(with = "clickhouse::serde::chrono::datetime64::millis")]
    received_at: chrono::DateTime<chrono::Utc>,
    outcome: i8,
    dedup_key: &'a str,
    #[serde(with = "clickhouse::serde::uuid")]
    event_id: Uuid,
    #[serde(with = "clickhouse::serde::uuid::option")]
    incident_id: Option<Uuid>,
    note: &'a str,
    body: &'a str,
    evidence_days: u16,
    ttl_days: u16,
}

impl<'a> AlertEventRow<'a> {
    /// `None` for an event whose source was deleted mid-request: nobody
    /// reads its log.
    fn from_delivery(r: &'a Delivery, ttl: RetentionDays) -> Option<Self> {
        let outcome = OUTCOMES.iter().find(|(s, _)| *s == r.status)?.1;
        Some(Self {
            org_id: r.org_id,
            source_id: r.source_id,
            received_at: r.received_at,
            outcome,
            dedup_key: &r.dedup_key,
            event_id: Uuid::now_v7(),
            incident_id: r.incident_id,
            note: &r.note,
            body: &r.body,
            evidence_days: ttl.evidence,
            ttl_days: ttl.row,
        })
    }
}
