//! Failure-shape gauges from ClickHouse, for catching a probe that fails on its
//! own account and keeps failing — a defect concentrated in one monitor rather
//! than spread across a fleet. Raw error strings cannot carry that signal: each
//! unique string trivially reads as one monitor at full share, so the fold is by
//! class.
//!
//! A control-plane task like `region_health`, deliberately cross-org: it watches
//! the fleet's probe quality, not a tenant's monitors, so it must never back a
//! request-path surface.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::Context;
use chrono::Utc;
use clickhouse::{Client as ChClient, Row};
use serde::Deserialize;
use tokio::time::{MissedTickBehavior, interval};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::domain::{ErrorClass, ErrorFamily, classify_check_error};
use crate::error::Result;
use crate::metric_names;

const TICK: Duration = Duration::from_secs(60);
/// A snapshot, not a verdict: how long a class must stay stuck before it pages
/// is the alert rule's `for`.
const WINDOW_SECS: u32 = 900;
/// Free-text errors are unbounded, so the tail is cut; a truncated sweep says so.
const MAX_ERROR_ROWS: u64 = 500;
// The CH client has no read timeout; bound the read so a hung (not refused)
// server can't stall the gauge loop until the next tick.
const QUERY_TIMEOUT: Duration = Duration::from_secs(10);
/// Picks which sweeps name a monitor in the log. The alert threshold lives in
/// the rule, not here.
const DOMINANT_SHARE: f64 = 0.9;

#[derive(Row, Deserialize, Debug, Clone, PartialEq)]
pub struct ErrorRow {
    pub error: Option<String>,
    pub checks: u64,
    pub top_monitor_checks: u64,
    #[serde(with = "clickhouse::serde::uuid")]
    pub top_monitor: Uuid,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ClassStat {
    pub class: ErrorClass,
    pub checks: u64,
    /// 0..1, and a lower bound: per-monitor counts are known only per raw error,
    /// so one monitor spanning several of a class's strings reads low. Costs
    /// sensitivity, never a false page.
    pub top_monitor_share: f64,
    pub top_monitor: Option<Uuid>,
}

pub async fn run(ch: ChClient, shutdown: CancellationToken) {
    let mut ticker = interval(TICK);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let started = Instant::now();
    let mut last_ok: Option<Instant> = None;
    // Establish both health series at boot. A rule on a series that does not
    // exist yet reads no_data, which is indistinguishable from healthy.
    metrics::gauge!(metric_names::CHECK_ERROR_CLASS_TRUNCATED).set(0.0);
    metrics::gauge!(metric_names::CHECK_ERROR_CLASS_SWEEP_AGE).set(0.0);
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => return,
            _ = ticker.tick() => {
                match sweep(&ch).await {
                    Ok(()) => last_ok = Some(Instant::now()),
                    Err(err) => {
                        tracing::warn!(?err, "error class gauge sweep failed; serving last values");
                    }
                }
                // Published even when the sweep fails: every gauge below holds
                // its last value on failure, so without an age the alert cannot
                // tell a quiet fleet from a sweep that stopped running.
                let age = last_ok.unwrap_or(started).elapsed().as_secs_f64();
                metrics::gauge!(metric_names::CHECK_ERROR_CLASS_SWEEP_AGE).set(age);
            }
        }
    }
}

async fn sweep(ch: &ChClient) -> Result<()> {
    let rows = match tokio::time::timeout(QUERY_TIMEOUT, collect(ch, WINDOW_SECS)).await {
        Ok(res) => res?,
        Err(_) => return Err(anyhow::anyhow!("error class sweep timed out").into()),
    };
    // Raw error strings interpolate hostnames and IPs, so their count grows
    // with the fleet and can push a fixed-string class off the end of the cap.
    // That would publish the class as 0 and leave its alert unfireable, so the
    // truncation is a series, not only a log line. Set only on a fresh read;
    // whether a held value is current is what the age gauge answers.
    let truncated = rows.len() as u64 >= MAX_ERROR_ROWS;
    if truncated {
        tracing::warn!(
            limit = MAX_ERROR_ROWS,
            "error class sweep hit the row cap; classes below the cut are undercounted"
        );
    }
    metrics::gauge!(metric_names::CHECK_ERROR_CLASS_TRUNCATED).set(u64::from(truncated) as f64);
    publish(&fold(rows));
    Ok(())
}

/// One class's gauge values for this sweep. The label set is closed by
/// construction: a class and its family, never an org, a monitor or raw error
/// text. `top_monitor` is log-only and deliberately not part of it.
#[derive(Debug, Clone, PartialEq)]
struct ClassSeries {
    class: ErrorClass,
    checks: f64,
    top_monitor_share: f64,
    top_monitor: Option<Uuid>,
}

/// Covers every class, not only the ones this window saw: an unwritten gauge
/// holds its last value and an alert on a frozen series never clears.
fn series(stats: &HashMap<ErrorClass, ClassStat>) -> Vec<ClassSeries> {
    ErrorClass::ALL
        .iter()
        .map(|class| {
            let stat = stats.get(class);
            ClassSeries {
                class: *class,
                checks: stat.map_or(0.0, |s| s.checks as f64),
                top_monitor_share: stat.map_or(0.0, |s| s.top_monitor_share),
                top_monitor: stat.and_then(|s| s.top_monitor),
            }
        })
        .collect()
}

fn publish(stats: &HashMap<ErrorClass, ClassStat>) {
    for s in series(stats) {
        let class = s.class.as_str();
        let family = s.class.family().as_str();
        metrics::gauge!(metric_names::CHECK_ERROR_CLASS_CHECKS, "class" => class, "family" => family)
            .set(s.checks);
        metrics::gauge!(metric_names::CHECK_ERROR_CLASS_TOP_MONITOR_SHARE, "class" => class, "family" => family)
            .set(s.top_monitor_share);
        // The gauges are anonymous by design, so this line is the only bridge
        // from a stuck class to the monitor behind it.
        if s.class.family() == ErrorFamily::Internal
            && s.top_monitor_share >= DOMINANT_SHARE
            && let Some(monitor) = s.top_monitor
        {
            tracing::warn!(
                class,
                checks = s.checks,
                share = s.top_monitor_share,
                target_id = %monitor,
                "one monitor accounts for nearly every check in a probe-side error class"
            );
        }
    }
}

fn fold(rows: Vec<ErrorRow>) -> HashMap<ErrorClass, ClassStat> {
    let mut totals: HashMap<ErrorClass, (u64, u64, Option<Uuid>)> = HashMap::new();
    for row in rows {
        let Some(raw) = row.error.as_deref().filter(|e| !e.is_empty()) else {
            continue;
        };
        let entry = totals
            .entry(classify_check_error(raw))
            .or_insert((0, 0, None));
        entry.0 += row.checks;
        if row.top_monitor_checks > entry.1 {
            entry.1 = row.top_monitor_checks;
            entry.2 = Some(row.top_monitor);
        }
    }
    totals
        .into_iter()
        .map(|(class, (checks, top_checks, top_monitor))| {
            let stat = ClassStat {
                class,
                checks,
                top_monitor_share: if checks == 0 {
                    0.0
                } else {
                    top_checks as f64 / checks as f64
                },
                top_monitor,
            };
            (class, stat)
        })
        .collect()
}

/// Raw results, not the per-minute rollup, which carries no error column.
/// `timestamp` leads the partition key, so the window prunes to a day or two
/// even though the scan crosses every org.
pub async fn collect(ch: &ChClient, window_secs: u32) -> Result<Vec<ErrorRow>> {
    let cutoff = Utc::now().timestamp() - i64::from(window_secs);
    let rows = ch
        .query(
            "SELECT error, \
                    sum(per_target) AS checks, \
                    max(per_target) AS top_monitor_checks, \
                    argMax(target_id, per_target) AS top_monitor \
             FROM ( \
                 SELECT error, target_id, count() AS per_target \
                 FROM check_results \
                 WHERE timestamp >= fromUnixTimestamp(?) \
                   AND error IS NOT NULL AND error != '' \
                 GROUP BY error, target_id \
             ) \
             GROUP BY error \
             ORDER BY checks DESC \
             LIMIT ?",
        )
        .bind(cutoff)
        .bind(MAX_ERROR_ROWS)
        .fetch_all::<ErrorRow>()
        .await
        .context("clickhouse error class sweep")?;
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(error: &str, checks: u64, top: u64, monitor: Uuid) -> ErrorRow {
        ErrorRow {
            error: Some(error.to_string()),
            checks,
            top_monitor_checks: top,
            top_monitor: monitor,
        }
    }

    #[test]
    fn folds_distinct_raw_errors_into_one_class() {
        let a = Uuid::from_u128(1);
        let b = Uuid::from_u128(2);
        let stats = fold(vec![
            row("unexpected status 403", 30, 20, a),
            row("unexpected status 500", 10, 6, b),
        ]);
        let stat = &stats[&ErrorClass::UnexpectedStatus];
        assert_eq!(stat.checks, 40);
        assert_eq!(stat.top_monitor, Some(a));
        assert!((stat.top_monitor_share - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn a_class_owned_by_one_monitor_reads_as_full_share() {
        let a = Uuid::from_u128(7);
        let stats = fold(vec![row(
            "flow engine not configured on this node",
            4319,
            4319,
            a,
        )]);
        let stat = &stats[&ErrorClass::FlowNotConfigured];
        assert_eq!(stat.class.family(), ErrorFamily::Internal);
        assert!(stat.top_monitor_share >= DOMINANT_SHARE);
        assert_eq!(stat.top_monitor, Some(a));
    }

    #[test]
    fn a_class_spread_across_a_fleet_stays_quiet() {
        let stats = fold(vec![
            row("no response", 900, 30, Uuid::from_u128(1)),
            row("timeout", 500, 20, Uuid::from_u128(2)),
        ]);
        assert!(stats[&ErrorClass::NoResponse].top_monitor_share < DOMINANT_SHARE);
        assert_eq!(
            stats[&ErrorClass::NoResponse].class.family(),
            ErrorFamily::Transport
        );
    }

    #[test]
    fn share_reads_low_when_one_monitor_spans_several_strings_of_a_class() {
        let a = Uuid::from_u128(1);
        let stats = fold(vec![
            row("dns: lookup timed out", 50, 50, a),
            row("dns: lookup failed", 50, 50, a),
        ]);
        let stat = &stats[&ErrorClass::DnsFailed];
        assert_eq!(stat.checks, 100);
        assert!(
            (stat.top_monitor_share - 0.5).abs() < f64::EPSILON,
            "one monitor owning every string of a class still reads as a lower bound"
        );
    }

    #[test]
    fn series_reports_every_class_including_the_ones_this_window_missed() {
        let seen = fold(vec![row("body", 12, 9, Uuid::from_u128(3))]);
        let rows = series(&seen);

        assert_eq!(rows.len(), ErrorClass::ALL.len());
        for class in ErrorClass::ALL {
            assert!(
                rows.iter().any(|r| r.class == *class),
                "{} would hold its last value forever",
                class.as_str()
            );
        }
        let body = rows.iter().find(|r| r.class == ErrorClass::Body).unwrap();
        assert_eq!(body.checks, 12.0);
        let quiet = rows.iter().find(|r| r.class == ErrorClass::Tls).unwrap();
        assert_eq!(quiet.checks, 0.0);
        assert_eq!(quiet.top_monitor_share, 0.0);
        assert_eq!(quiet.top_monitor, None);
    }

    #[test]
    fn a_class_that_stops_occurring_is_zeroed_not_dropped() {
        let rows = series(&fold(Vec::new()));
        assert_eq!(rows.len(), ErrorClass::ALL.len());
        assert!(
            rows.iter()
                .all(|r| r.checks == 0.0 && r.top_monitor_share == 0.0)
        );
    }

    #[test]
    fn empty_and_null_errors_are_dropped() {
        let stats = fold(vec![
            row("", 5, 5, Uuid::nil()),
            ErrorRow {
                error: None,
                checks: 5,
                top_monitor_checks: 5,
                top_monitor: Uuid::nil(),
            },
        ]);
        assert!(stats.is_empty());
    }
}
