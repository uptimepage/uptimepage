//! Outbound dead-man's-switch: pings an external watcher (config key
//! `observability.heartbeat`) so a dead or dependency-blind control plane is
//! noticed from outside. Distinct from the heartbeat *monitor kind*, which is
//! customers' inbound pings, hence "snitch".

use std::sync::Arc;
use std::time::Duration;

use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::config::HeartbeatConfig;
use crate::http_outbound::{self, OutboundHttpClient};
use crate::observability::readiness::probe_readiness;
use crate::storage::{ResultsStore, TargetStore};

pub fn spawn(
    cfg: &HeartbeatConfig,
    client: OutboundHttpClient,
    target_store: Arc<dyn TargetStore>,
    results_store: Arc<dyn ResultsStore>,
    cancel: CancellationToken,
) -> Option<JoinHandle<()>> {
    if !cfg.enabled {
        return None;
    }
    let url = match Url::parse(&cfg.url) {
        Ok(u) => u,
        Err(e) => {
            tracing::error!(error = %e, "snitch disabled: invalid observability.heartbeat.url");
            return None;
        }
    };
    let interval = Duration::from_secs(cfg.interval_seconds.max(1));
    Some(tokio::spawn(run(
        client,
        url,
        interval,
        target_store,
        results_store,
        cancel,
    )))
}

async fn run(
    client: OutboundHttpClient,
    url: Url,
    interval: Duration,
    target_store: Arc<dyn TargetStore>,
    results_store: Arc<dyn ResultsStore>,
    cancel: CancellationToken,
) {
    // URL embeds a capability token — log only the host, never the full URL.
    let host = url.host_str().unwrap_or("?").to_owned();
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = ticker.tick() => {
                // Skip the ping when a dependency is down so the external watcher
                // alerts on partial outages, not just a dead process.
                let ready = probe_readiness(&target_store, &results_store).await;
                if !ready.all_ok() {
                    tracing::warn!(
                        postgres = ready.postgres,
                        clickhouse = ready.clickhouse,
                        "snitch: dependency down, withholding ping"
                    );
                    continue;
                }
                if http_outbound::get_ok(&client, &url).await.is_err() {
                    tracing::warn!(host = %host, "snitch: ping failed");
                }
            }
        }
    }
}
