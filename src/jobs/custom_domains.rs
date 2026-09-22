//! Keeps the served custom-domain snapshot current. A failed rebuild leaves
//! the last good snapshot live rather than denying: a fail-closed snapshot
//! would take a branded status page dark during the incident it exists for.
//! The cost is that a removal, a hold and a deletion land late while Postgres
//! is unreachable.

use std::sync::Arc;
use std::time::Duration;

use sqlx::PgPool;
use tokio::time::{MissedTickBehavior, interval};
use tokio_util::sync::CancellationToken;

use crate::metric_names;
use crate::request::custom_domains::CustomDomains;
use crate::storage::status_pages::load_verified_custom_domains;

const REFRESH_INTERVAL: Duration = Duration::from_secs(30);

/// Run before the listener binds, and abort the boot on failure. An instance
/// that binds with an empty snapshot passes its health check, joins the round
/// robin and 404s hostnames the other color serves. Aborting costs nothing new:
/// `connect_pool` already runs migrations against this pool and already aborts.
pub async fn load_before_serving(
    pool: &PgPool,
    domains: &CustomDomains,
) -> crate::error::Result<()> {
    let n = refresh(pool, domains).await?;
    tracing::info!(domains = n, "custom domain snapshot loaded");
    Ok(())
}

pub fn spawn(
    pool: PgPool,
    domains: Arc<CustomDomains>,
    shutdown: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = interval(REFRESH_INTERVAL);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
        // load_before_serving already spent this tick.
        ticker.tick().await;
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => return,
                _ = ticker.tick() => {
                    if let Err(err) = refresh(&pool, &domains).await {
                        tracing::warn!(error = %err, "custom domain snapshot refresh failed; serving the last good one");
                    }
                }
            }
        }
    })
}

pub async fn refresh(pool: &PgPool, domains: &CustomDomains) -> crate::error::Result<usize> {
    let _guard = domains.refresh_guard().await;
    let rows = load_verified_custom_domains(pool).await?;
    let n = domains.install(rows);
    metrics::gauge!(metric_names::CUSTOM_DOMAINS_SERVED).set(n as f64);
    if let Some(at) = domains.loaded_at() {
        metrics::gauge!(metric_names::CUSTOM_DOMAINS_UPDATED).set(at.timestamp() as f64);
    }
    Ok(n)
}
