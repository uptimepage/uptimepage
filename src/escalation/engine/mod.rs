use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use chrono::Utc;
use moka::future::Cache;
use tokio::sync::{Mutex, Semaphore, mpsc};
use tokio::time::MissedTickBehavior;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// Concurrent in-flight signal pages — bounds a signal storm so one tenant's
/// burst cannot spawn unbounded delivery tasks.
const SIGNAL_CONCURRENCY: usize = 16;
/// Incidents escalated/retried concurrently within a sweep, so one slow channel
/// cannot serialise the whole tick.
const SWEEP_CONCURRENCY: usize = 8;
/// Fixed pool of per-incident locks (sharded by incident id) that serialise all
/// paging for one incident across the concurrent signal + sweep tasks, so a
/// reconcile and an inbound signal cannot both open the same episode and
/// double-page. Two distinct incidents may share a shard (harmless contention).
const PAGE_LOCK_SHARDS: usize = 256;
/// On-call roster cache lifetime. Short enough that an edit or a handoff is
/// reflected within seconds; long enough to collapse a sweep's repeated
/// resolutions of the same schedule into one query.
const ON_CALL_CACHE_TTL_SECS: u64 = 15;

use metrics::counter;

use crate::config::EscalationConfig;
use crate::domain::{NotificationReason, OrgId, UserId};
use crate::http_outbound::OutboundHttpClient;
use crate::storage::orgs::OrgDirectory;
use crate::storage::{
    ContactStore, EscalationPolicyStore, IncidentOpsStore, NotificationChannelStore, OnCallStore,
    TargetStore,
};

use rules::{retry_after_hint, retry_delay_secs};

mod deliver;
mod episode;
pub mod rules;
mod sweep;
#[cfg(test)]
mod tests;

/// One resolved paging destination: a concrete channel plus, when the rung
/// targeted a person or schedule, the responder it resolved to (recorded on the
/// notification row for the audit trail).
#[derive(Clone, Copy)]
struct PageTarget {
    channel_id: Uuid,
    user_id: Option<UserId>,
}

/// A nudge that an incident's state changed and its paging should be
/// reconciled. Carries no payload beyond identity + the reason to page; the
/// engine re-reads the incident, monitor, and channels so a stale signal never
/// pages outdated content.
pub struct IncidentSignal {
    pub org: OrgId,
    pub incident_id: Uuid,
    pub reason: NotificationReason,
}

/// Owns the rx loop. The paging work lives behind an `Arc<Worker>` so the loop
/// can dispatch signal handling and the periodic sweep onto detached tasks —
/// a slow channel can never stall `rx.recv()` (and with it every other
/// tenant's pages) the way an inline await would.
pub struct EscalationEngine {
    rx: mpsc::Receiver<IncidentSignal>,
    w: Arc<Worker>,
}

/// Everything the engine pages with, gathered so the constructor stays
/// readable at the call site.
pub struct EngineDeps {
    pub ops: Arc<dyn IncidentOpsStore>,
    pub policies: Arc<dyn EscalationPolicyStore>,
    pub on_call: Arc<dyn OnCallStore>,
    pub contacts: Arc<dyn ContactStore>,
    pub targets: Arc<dyn TargetStore>,
    pub channels: Arc<dyn NotificationChannelStore>,
    pub maintenance: Arc<dyn crate::storage::MaintenanceStore>,
    pub orgs: Arc<dyn OrgDirectory>,
    pub http: OutboundHttpClient,
    pub cfg: EscalationConfig,
    /// Operator base URL for incident deep links; empty omits the link.
    pub base_url: String,
    /// Keys the one-click stop link in alert mail; empty omits the link.
    pub alert_channel_stop_secret: String,
    /// Keys the acknowledge link pushed to phones; empty omits the link.
    pub incident_ack_secret: String,
    /// Operator token + shared send budget for `telegram_app` delivery.
    pub central_bot: Option<crate::notifier::CentralBotDelivery>,
    /// Operator Cloud API credentials for `whatsapp_app` delivery.
    pub central_whatsapp: Option<crate::config::WhatsAppAppBotConfig>,
    /// Transactional sender + From identity for `email` delivery.
    pub email: Option<crate::notifier::EmailDelivery>,
}

/// The shared paging core. Every field is cheap to clone/share; methods take
/// `&self` so they run from any task holding the `Arc`.
struct Worker {
    ops: Arc<dyn IncidentOpsStore>,
    policies: Arc<dyn EscalationPolicyStore>,
    on_call: Arc<dyn OnCallStore>,
    contacts: Arc<dyn ContactStore>,
    targets: Arc<dyn TargetStore>,
    channels: Arc<dyn NotificationChannelStore>,
    maintenance: Arc<dyn crate::storage::MaintenanceStore>,
    orgs: Arc<dyn OrgDirectory>,
    http: OutboundHttpClient,
    cfg: EscalationConfig,
    base_url: String,
    alert_channel_stop_secret: String,
    incident_ack_secret: String,
    central_bot: Option<crate::notifier::CentralBotDelivery>,
    central_whatsapp: Option<crate::config::WhatsAppAppBotConfig>,
    email: Option<crate::notifier::EmailDelivery>,
    /// True while a sweep task is in flight, so overlapping ticks skip rather
    /// than stack a second sweep on top of a slow one.
    sweeping: AtomicBool,
    /// Sharded per-incident locks; `page()` holds one for the incident it
    /// touches so concurrent signal + sweep tasks serialise per incident.
    page_locks: Vec<Mutex<()>>,
    /// Short-TTL cache of resolved on-call rosters, keyed by schedule. Who is
    /// on call only changes at a handoff (>= daily), so a correlated outage
    /// paging many incidents off one schedule resolves it once per window
    /// instead of running the multi-query load per incident every tick.
    on_call_cache: Cache<(OrgId, Uuid), Arc<Vec<UserId>>>,
}

impl EscalationEngine {
    pub fn new(rx: mpsc::Receiver<IncidentSignal>, deps: EngineDeps) -> Self {
        let EngineDeps {
            ops,
            policies,
            on_call,
            contacts,
            targets,
            channels,
            maintenance,
            orgs,
            http,
            cfg,
            base_url,
            alert_channel_stop_secret,
            incident_ack_secret,
            central_bot,
            central_whatsapp,
            email,
        } = deps;
        Self {
            rx,
            w: Arc::new(Worker {
                ops,
                policies,
                on_call,
                contacts,
                targets,
                channels,
                maintenance,
                orgs,
                http,
                cfg,
                base_url,
                alert_channel_stop_secret,
                incident_ack_secret,
                central_bot,
                central_whatsapp,
                email,
                sweeping: AtomicBool::new(false),
                page_locks: (0..PAGE_LOCK_SHARDS).map(|_| Mutex::new(())).collect(),
                on_call_cache: Cache::builder()
                    .max_capacity(10_000)
                    .time_to_live(Duration::from_secs(ON_CALL_CACHE_TTL_SECS))
                    .build(),
            }),
        }
    }

    pub async fn run(mut self, shutdown: CancellationToken) {
        let mut tick =
            tokio::time::interval(Duration::from_secs(self.w.cfg.tick_interval_secs.max(1)));
        tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
        // Bounds concurrent signal-driven pages so a burst can't spawn without
        // limit; the sweep self-limits via SWEEP_CONCURRENCY + the budget.
        let sig_sem = Arc::new(Semaphore::new(SIGNAL_CONCURRENCY));
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => return,
                maybe = self.rx.recv() => match maybe {
                    Some(sig) => {
                        let w = self.w.clone();
                        let sem = sig_sem.clone();
                        tokio::spawn(async move {
                            let _permit = sem.acquire().await;
                            if let Err(err) = w.page(sig.org, sig.incident_id, sig.reason).await {
                                tracing::warn!(incident_id = %sig.incident_id, error = %err, "incident paging failed");
                            }
                        });
                    }
                    None => return,
                },
                _ = tick.tick() => {
                    // Only one sweep at a time; a long sweep makes the next tick
                    // a no-op rather than piling on.
                    if self.w.sweeping.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire).is_ok() {
                        let w = self.w.clone();
                        tokio::spawn(async move {
                            // Reset the flag on completion AND on unwind, so a
                            // panic in a sweep never wedges the engine into
                            // "always sweeping" (which would silently kill all
                            // future escalation/retry/reconcile).
                            struct ResetOnDrop(Arc<Worker>);
                            impl Drop for ResetOnDrop {
                                fn drop(&mut self) {
                                    self.0.sweeping.store(false, Ordering::Release);
                                }
                            }
                            let _reset = ResetOnDrop(w.clone());
                            w.sweep().await;
                        });
                    }
                }
            }
        }
    }

    // Test entry points — production drives these through `run` + the sweep.
    #[cfg(test)]
    async fn page(
        &self,
        org: OrgId,
        incident_id: Uuid,
        reason: NotificationReason,
    ) -> crate::error::Result<()> {
        self.w.page(org, incident_id, reason).await
    }
    #[cfg(test)]
    async fn page_is_spent(&self, ack: &crate::storage::EmergencyAck) -> bool {
        self.w.page_is_spent(ack).await
    }
    #[cfg(test)]
    async fn escalate_due(&self) {
        self.w.escalate_due().await
    }
    #[cfg(test)]
    async fn retry_pending(&self) {
        self.w.retry_pending().await
    }
    #[cfg(test)]
    async fn release_held(&self) {
        self.w.release_held().await
    }

    #[cfg(test)]
    async fn release_maintenance(&self) {
        self.w.release_maintenance().await
    }
    /// The paging half of a release, so a test can drive the window between
    /// the scan and the page that the scan's own state filter hides.
    #[cfg(test)]
    async fn release_page(&self, org: OrgId, incident_id: Uuid) -> crate::error::Result<()> {
        self.w
            .page_with(
                org,
                incident_id,
                NotificationReason::Opened,
                super::engine::rules::Damper::Skip,
            )
            .await
    }
    #[cfg(test)]
    async fn reconcile(&self) {
        self.w.reconcile().await
    }
    #[cfg(test)]
    async fn renotify_one(&self, d: &crate::storage::DueIncident) -> crate::error::Result<()> {
        self.w.renotify_one(d).await
    }
}

impl Worker {
    /// Per-sweep wall-clock ceiling: a sweep never runs longer than one tick, so
    /// the loop returns to draining `rx` promptly even under slow channels.
    fn sweep_budget(&self) -> Duration {
        Duration::from_secs(self.cfg.tick_interval_secs.max(1))
    }

    /// Wall-clock time of the next retry after `attempt` just failed, or `None`
    /// once the attempt cap is reached (the row is dead-lettered). Adds up to
    /// +50% random jitter so a correlated burst of failures doesn't retry in
    /// lockstep against a recovering endpoint.
    fn retry_backoff(&self, attempt: i32) -> Option<chrono::DateTime<Utc>> {
        retry_delay_secs(
            attempt,
            self.cfg.retry_backoff_base_secs,
            self.cfg.retry_backoff_cap_secs,
            self.cfg.max_attempts,
        )
        .map(|secs| {
            let jitter = fastrand::u64(0..=secs / 2 + 1);
            Utc::now() + chrono::Duration::seconds((secs + jitter) as i64)
        })
    }

    /// Backoff raised to the transport's own retry hint when the error
    /// carries one; exhausted attempts stay dead-lettered regardless.
    fn retry_backoff_hinted(
        &self,
        attempt: i32,
        error: Option<&str>,
    ) -> Option<chrono::DateTime<Utc>> {
        let at = self.retry_backoff(attempt)?;
        Some(match retry_after_hint(error) {
            Some(wait) => at.max(Utc::now() + wait),
            None => at,
        })
    }

    /// Count a page that exhausted its retries (no further attempt scheduled),
    /// so a systemic delivery failure shows up as a metric, not just per-incident.
    fn note_dead_letter(&self, transport: &str, next_attempt_at: Option<chrono::DateTime<Utc>>) {
        if next_attempt_at.is_none() {
            counter!(
                crate::metric_names::NOTIFICATIONS_DEAD_LETTERED,
                "transport" => transport.to_string()
            )
            .increment(1);
        }
    }

    /// The shard lock guarding paging for `incident_id`.
    fn page_lock(&self, incident_id: Uuid) -> &Mutex<()> {
        let shard = (incident_id.as_u128() % PAGE_LOCK_SHARDS as u128) as usize;
        &self.page_locks[shard]
    }
}
