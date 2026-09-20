//! Whether the stores answer, for `/readyz` and the dead-man heartbeat.

use std::sync::Arc;

use crate::storage::{ResultsStore, TargetStore};

/// Per-dependency readiness snapshot. Both critical stores must answer for
/// the app to be "ready". Drives `/readyz` and the external heartbeat.
#[derive(Debug, Clone, Copy)]
pub struct Readiness {
    pub postgres: bool,
    pub clickhouse: bool,
}

impl Readiness {
    pub fn all_ok(&self) -> bool {
        self.postgres && self.clickhouse
    }
}

/// A dependency that doesn't answer within this is "down" — a TCP-alive but
/// hung store must not wedge `/readyz` (and the heartbeat tick) forever. Kept
/// under the deploy cutover gate's 5s `wget -T 5` so a hung store yields a
/// clean per-dependency 503 instead of racing the prober's own timeout.
const READINESS_PING_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(4);

/// Ping every critical dependency concurrently — connection-level only,
/// never tenant data. Single source of truth for "ready": `/readyz` returns
/// a per-dependency 503 from it, and the dead-man's-switch heartbeat skips
/// its external ping when this is not `all_ok` (so the snitch fires on a
/// dependency outage, not just a full process death).
pub async fn probe_readiness(
    target_store: &Arc<dyn TargetStore>,
    results_store: &Arc<dyn ResultsStore>,
) -> Readiness {
    let (postgres, clickhouse) = tokio::join!(
        ping_dependency("postgres", target_store.ping()),
        ping_dependency("clickhouse", results_store.ping()),
    );
    Readiness {
        postgres,
        clickhouse,
    }
}

async fn ping_dependency<E: std::fmt::Debug>(
    name: &str,
    ping: impl std::future::Future<Output = std::result::Result<(), E>>,
) -> bool {
    match tokio::time::timeout(READINESS_PING_TIMEOUT, ping).await {
        Ok(Ok(())) => true,
        Ok(Err(e)) => {
            tracing::warn!(dependency = name, error = ?e, "readiness ping failed");
            false
        }
        Err(_) => {
            tracing::warn!(dependency = name, "readiness ping timed out");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::{InMemorySink, InMemoryTargetStore};

    #[tokio::test]
    async fn probe_readiness_reports_reachable_stores_as_up() {
        let ts: Arc<dyn TargetStore> = Arc::new(InMemoryTargetStore::new());
        let rs: Arc<dyn ResultsStore> = Arc::new(InMemorySink::new());
        let r = probe_readiness(&ts, &rs).await;
        assert!(r.all_ok());
        assert!(r.postgres && r.clickhouse);
    }

    #[test]
    fn all_ok_requires_every_dependency() {
        assert!(
            Readiness {
                postgres: true,
                clickhouse: true
            }
            .all_ok()
        );
        assert!(
            !Readiness {
                postgres: true,
                clickhouse: false
            }
            .all_ok()
        );
        assert!(
            !Readiness {
                postgres: false,
                clickhouse: true
            }
            .all_ok()
        );
    }
}
