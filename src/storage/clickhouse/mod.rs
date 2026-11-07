//! ClickHouse storage: the migration runner, the write sinks, and the
//! org-scoped read side, over the shared client and rollup-window helpers here.

use std::time::Duration;

use anyhow::Context;
use backoff::ExponentialBackoffBuilder;
use chrono::{DateTime, TimeZone, Utc};
use clickhouse::{Client, Row, RowOwned, RowRead, query::Query};
use secrecy::ExposeSecret;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::config::ClickhouseConfig;
use crate::error::Result;

mod results;
mod schema;
mod sinks;

pub use results::ClickhouseResultsStore;
pub use schema::migrate;
pub use sinks::{
    ClickhouseAlertEventSink, ClickhouseFlowRunSink, ClickhouseHeartbeatPingSink,
    ClickhouseResultSink,
};

const TABLE: &str = "check_results";
const FLOW_TABLE: &str = "flow_runs";
const HEARTBEAT_PING_TABLE: &str = "heartbeat_pings";
const ALERT_EVENT_TABLE: &str = "alert_events";

/// Seconds bound for the matview `minute` column (`DateTime`).
#[derive(Serialize)]
struct MinuteBound(u32);

impl MinuteBound {
    fn new(dt: DateTime<Utc>) -> Self {
        Self(to_unix_secs(dt))
    }
}

/// `timestamp`/`ingested_at` are `DateTime` (UInt32 seconds on the wire).
fn to_unix_secs(dt: DateTime<Utc>) -> u32 {
    dt.timestamp().clamp(0, i64::from(u32::MAX)) as u32
}

fn from_unix_secs(secs: u32) -> DateTime<Utc> {
    Utc.timestamp_opt(i64::from(secs), 0)
        .single()
        .unwrap_or_else(Utc::now)
}

/// `[from, to)` over the matview `minute` column; bind via [`bind_minute_window`].
const MINUTE_WINDOW: &str = "minute >= fromUnixTimestamp(?) AND minute < fromUnixTimestamp(?)";

/// 1m rollup TTL (days). Ranges reaching past it read the 1h rollup instead.
const MINUTE_ROLLUP_DAYS: i64 = 30;

/// The two [`MinuteBound`] binds [`MINUTE_WINDOW`] expects; call after the
/// leading positional binds (org_id, target_id, …).
fn bind_minute_window(q: Query, from: DateTime<Utc>, to: DateTime<Utc>) -> Query {
    q.bind(MinuteBound::new(from)).bind(MinuteBound::new(to))
}

/// `(table, time_column)` for a range: the 1m rollup within its TTL, else the
/// 1h rollup. Both carry the same AggregateFunction columns, so a merge query
/// reads either by swapping these two tokens.
fn rollup_source(from: DateTime<Utc>) -> (&'static str, &'static str) {
    if from >= Utc::now() - chrono::Duration::days(MINUTE_ROLLUP_DAYS) {
        ("check_results_1m", "minute")
    } else {
        ("check_results_1h", "hour")
    }
}
pub fn build_client(cfg: &ClickhouseConfig) -> Client {
    let mut client = Client::default()
        .with_url(&cfg.url)
        .with_database(&cfg.database)
        .with_user(&cfg.user);
    if !cfg.password.expose_secret().is_empty() {
        client = client.with_password(cfg.password.expose_secret());
    }
    if cfg.async_insert {
        // INSERT-only settings; SELECTs ignore them. `wait_for_async_insert`
        // keeps end() returning only after a durable flush. `async_insert`
        // coalesces server-side, which forms its own blocks — so the raw
        // MergeTree block dedup no longer recognises a re-sent batch;
        // `async_insert_deduplicate` restores idempotency for the batcher's
        // identical-block retry.
        client = client
            .with_setting("async_insert", "1")
            .with_setting("wait_for_async_insert", "1")
            .with_setting("async_insert_deduplicate", "1");
    }
    client
}

/// Unbounded live row count for one org in a single ClickHouse table. Used by
/// the GDPR purge to prove an erasure actually landed (zero == erased), so it
/// deliberately takes no time range — it must see *every* surviving row.
/// `table` is always a fixed in-crate constant at the call sites, never user
/// input, so the identifier interpolation is injection-free.
pub(crate) async fn count_org_rows(client: &Client, table: &str, org_id: Uuid) -> Result<u64> {
    let query = client
        .query(&format!(
            "SELECT count() AS n FROM {table} WHERE org_id = ?"
        ))
        .bind(org_id);
    let row: CountRow = fetch_single(query)
        .await
        .with_context(|| format!("clickhouse count_org_rows {table}"))?;
    Ok(row.n)
}

/// The first row of a query that returns at most one, read with the rest of
/// the response. The client's `fetch_one` and `fetch_optional` return after
/// the first row and leave the rest unread, which can close the connection
/// while ClickHouse is still writing; it logs that as a broken pipe.
pub(crate) async fn fetch_first<T: RowOwned + RowRead>(
    query: Query,
) -> clickhouse::error::Result<Option<T>> {
    let mut cursor = query.fetch::<T>()?;
    let first = cursor.next().await?;
    while cursor.next().await?.is_some() {}
    Ok(first)
}

/// [`fetch_first`] for a query that always returns a row, such as an
/// aggregate without `GROUP BY`.
pub(crate) async fn fetch_single<T: RowOwned + RowRead>(
    query: Query,
) -> clickhouse::error::Result<T> {
    fetch_first(query)
        .await?
        .ok_or(clickhouse::error::Error::RowNotFound)
}

/// Single-column scalar count row, shared by every `SELECT count() AS n …`
/// call in this module so each callsite doesn't redefine its own struct.
#[derive(Row, Deserialize)]
struct CountRow {
    n: u64,
}

/// Retry budget for an insert. Bounded well under the agent's own request
/// timeout so a stuck ClickHouse surfaces as a failed write rather than a
/// hanging caller.
fn insert_backoff() -> backoff::ExponentialBackoff {
    ExponentialBackoffBuilder::new()
        .with_initial_interval(Duration::from_millis(100))
        .with_max_interval(Duration::from_secs(5))
        .with_max_elapsed_time(Some(Duration::from_secs(30)))
        .build()
}
