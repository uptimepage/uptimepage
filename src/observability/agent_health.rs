//! Dead-man's-switch for regional agents. A control-plane task that periodically
//! reads `agents.last_seen_at` and exposes per-agent staleness as Prometheus
//! gauges (alertable in Grafana) plus a warn log on the fresh→stale transition.
//! Agents are operator/cross-tenant, so there is no per-org incident here.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use chrono::{DateTime, Utc};
use tokio::time::{MissedTickBehavior, interval};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::error::Result;
use crate::metric_names;
use crate::storage::operator::{AgentRow, OperatorRepo};

const TICK: Duration = Duration::from_secs(30);

pub async fn run(repo: OperatorRepo, stale_after: Duration, shutdown: CancellationToken) {
    let mut ticker = interval(TICK);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut stale: HashSet<Uuid> = HashSet::new();
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => return,
            _ = ticker.tick() => {
                if let Err(err) = sweep(&repo, stale_after, &mut stale).await {
                    tracing::warn!(error = %err, "agent health sweep failed");
                }
            }
        }
    }
}

async fn sweep(
    repo: &OperatorRepo,
    stale_after: Duration,
    stale: &mut HashSet<Uuid>,
) -> Result<()> {
    let agents = repo.list_agents().await?;
    let now = Utc::now();
    let mut live: HashSet<Uuid> = HashSet::new();
    for a in &agents {
        live.insert(a.id);
        // Never-reported agents age from creation.
        let since = a.last_seen_at.unwrap_or(a.created_at);
        let age = (now - since).num_seconds().max(0);
        let is_stale = age as u64 > stale_after.as_secs();
        // Emit for every agent (incl. disabled) so one that goes dark ages to 0.
        // Per-agent series can freeze if an agent is removed (the metrics facade
        // can't retract); that's fine for dashboards, and alerts page on the
        // freeze-proof aggregate below instead.
        metrics::gauge!(metric_names::AGENT_LAST_SEEN_AGE, "region" => a.region.clone(), "agent" => a.name.clone())
            .set(age as f64);
        metrics::gauge!(metric_names::AGENT_UP, "region" => a.region.clone(), "agent" => a.name.clone())
            .set(if is_stale { 0.0 } else { 1.0 });
        // Warn only when an *enabled* agent crosses into stale — a disabled one
        // is intentionally dark, not an incident.
        if is_stale && a.enabled {
            if stale.insert(a.id) {
                tracing::warn!(region = %a.region, agent = %a.name, age_secs = age, "regional agent is stale (no check-in)");
            }
        } else {
            stale.remove(&a.id);
        }
    }
    // Forget vanished agents so the log-dedup set can't grow unbounded.
    stale.retain(|id| live.contains(id));
    // Recomputed every sweep, so a recovered, disabled, or removed agent drops
    // out and the gauge can't latch. The count of enabled agents currently
    // dark, which the dead-man alert pages on.
    metrics::gauge!(metric_names::AGENTS_ENABLED_DOWN).set(stale.len() as f64);
    // Per-region quorum: fresh enabled agents out of the region's roster. Like
    // the per-agent gauges this can freeze if a region's last agent is removed;
    // dashboards read up-of-total and alert on up == 0 (region dark).
    for (region, total, up) in region_quorum(&agents, now, stale_after) {
        metrics::gauge!(metric_names::REGION_AGENTS_TOTAL, "region" => region.to_string())
            .set(total as f64);
        metrics::gauge!(metric_names::REGION_AGENTS_UP, "region" => region.to_string())
            .set(up as f64);
    }
    Ok(())
}

/// Per-region quorum from a roster: `(region, enabled_total, fresh_up)`.
/// Enabled agents only — a disabled agent is intentionally dark, not part of a
/// region's expected roster. Pure, so the quorum logic is unit-testable without
/// a metrics recorder.
fn region_quorum(
    agents: &[AgentRow],
    now: DateTime<Utc>,
    stale_after: Duration,
) -> Vec<(&str, u32, u32)> {
    let mut total: HashMap<&str, u32> = HashMap::new();
    let mut up: HashMap<&str, u32> = HashMap::new();
    for a in agents.iter().filter(|a| a.enabled) {
        *total.entry(a.region.as_str()).or_insert(0) += 1;
        let since = a.last_seen_at.unwrap_or(a.created_at);
        let age = (now - since).num_seconds().max(0);
        if age as u64 <= stale_after.as_secs() {
            *up.entry(a.region.as_str()).or_insert(0) += 1;
        }
    }
    total
        .into_iter()
        .map(|(region, t)| (region, t, up.get(region).copied().unwrap_or(0)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(region: &str, enabled: bool, last_seen_secs_ago: i64) -> AgentRow {
        let now = Utc::now();
        AgentRow {
            id: Uuid::now_v7(),
            region: region.into(),
            name: format!("{region}-agent"),
            enabled,
            token_prefix: "tok".into(),
            last_seen_at: Some(now - chrono::Duration::seconds(last_seen_secs_ago)),
            created_at: now,
        }
    }

    fn quorum_for<'a>(out: &'a [(&'a str, u32, u32)], region: &str) -> Option<(u32, u32)> {
        out.iter()
            .find(|(r, _, _)| *r == region)
            .map(|(_, t, u)| (*t, *u))
    }

    #[test]
    fn counts_fresh_enabled_agents_per_region() {
        let now = Utc::now();
        let stale_after = Duration::from_secs(90);
        let agents = vec![
            agent("eu", true, 10),  // fresh
            agent("eu", true, 30),  // fresh
            agent("eu", true, 600), // stale
            agent("us", true, 5),   // fresh
        ];
        let out = region_quorum(&agents, now, stale_after);
        assert_eq!(quorum_for(&out, "eu"), Some((3, 2)), "eu: 2 of 3 fresh");
        assert_eq!(quorum_for(&out, "us"), Some((1, 1)), "us: 1 of 1 fresh");
    }

    #[test]
    fn disabled_agents_are_not_part_of_the_roster() {
        let now = Utc::now();
        let stale_after = Duration::from_secs(90);
        let agents = vec![agent("eu", false, 5), agent("eu", true, 5)];
        // Only the enabled agent counts toward total and up.
        assert_eq!(
            quorum_for(&region_quorum(&agents, now, stale_after), "eu"),
            Some((1, 1))
        );
    }

    #[test]
    fn fully_stale_region_reports_zero_up() {
        let now = Utc::now();
        let stale_after = Duration::from_secs(90);
        let agents = vec![agent("eu", true, 600), agent("eu", true, 1200)];
        assert_eq!(
            quorum_for(&region_quorum(&agents, now, stale_after), "eu"),
            Some((2, 0)),
            "region dark: 0 of 2 fresh"
        );
    }
}
