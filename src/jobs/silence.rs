//! Per-monitor silence (no-data) detection + customer notification.
//!
//! A monitor goes truly silent only when the agents covering all its regions
//! stop running — a failing or timing-out check still reports an Error and opens
//! a normal incident. Each tick, the sweep derives the unmonitored set (one SQL
//! query over agent liveness + region assignment, excluding maintenance), records
//! the open/resolved boundary in `monitor_silence_state`, exposes an operator
//! gauge, and notifies the customer's bound channels once per episode.
//!
//! Sibling to `agent_health`: that one alarms a dead *agent* (operator-facing);
//! this rolls liveness up to the *monitor* so the customer whose monitor is now
//! blind is told. Delivery reuses the public notifier transports; it does not
//! touch the incident/escalation path (silence has no incident row).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use tokio::time::{MissedTickBehavior, interval};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::config::WhatsAppAppBotConfig;
use crate::domain::quota::RegionCaps;
use crate::domain::{
    IncidentOrigin, IncidentSeverity, IncidentUrgency, NotificationReason, OrgId, TargetAlerts,
    member_list,
};
use crate::error::Result;
use crate::http_outbound::OutboundHttpClient;
use crate::metric_names;
use crate::notifier::event::IncidentNotice;
use crate::notifier::{CentralBotDelivery, EmailDelivery, build_notifier};
use crate::quotas::QuotaService;
use crate::storage::{
    GroupSilence, NotificationChannelStore, Resolved, SilenceGroup, SilenceStore, TargetStore,
};

const TICK: Duration = Duration::from_secs(30);
/// Above this fraction of all enabled monitors silent at once, treat it as one
/// probe/infra outage (a region died): state is still recorded and the operator
/// alarmed, but per-customer notices are suppressed so our own outage never
/// storms every customer.
const MAX_SILENCE_FRACTION: f64 = 0.25;

/// Delivers a silence notice to a monitor's bound channels. Trait so the sweep's
/// delivery decisions are testable without live transports.
#[async_trait]
pub trait SilenceDelivery: Send + Sync {
    /// Returns whether the notice reached at least one channel.
    async fn notify(&self, org: OrgId, target_id: Uuid, reason: NotificationReason) -> bool;
    /// The channels a group's silence notices go to: its bindings and tag
    /// rules, as its pages resolve them.
    async fn group_channels(&self, org: OrgId, group: &SilenceGroup) -> Vec<Uuid>;
    /// One notice for a group's silence episode, keyed by the episode, to the
    /// channels it opened with. Returns whether it reached one.
    async fn notify_group(&self, silence: &GroupSilence, reason: NotificationReason) -> bool;
}

/// Production delivery over the public notifier transports. Holds the same
/// shared deps the escalation engine uses; builds no incident notification rows
/// (silence dedup lives in `monitor_silence_state`).
pub struct SilenceNotifier {
    pub channels: Arc<dyn NotificationChannelStore>,
    pub targets: Arc<dyn TargetStore>,
    pub orgs: Arc<dyn crate::storage::orgs::OrgDirectory>,
    pub http: OutboundHttpClient,
    pub central_bot: Option<CentralBotDelivery>,
    pub central_whatsapp: Option<WhatsAppAppBotConfig>,
    pub email: Option<EmailDelivery>,
    pub base_url: String,
    pub alert_channel_stop_secret: String,
}

fn silence_notice(
    key: Uuid,
    name: String,
    reason: NotificationReason,
    note: Option<String>,
) -> IncidentNotice {
    IncidentNotice {
        incident_id: key,
        reason,
        monitor_name: Some(name),
        title: None,
        severity: IncidentSeverity::default(),
        urgency: IncidentUrgency::default(),
        origin: IncidentOrigin::Monitor,
        started_at: Utc::now(),
        ended_at: None,
        error_sample: None,
        regions_down: Vec::new(),
        regions_up: Vec::new(),
        url: None,
        note,
    }
}

impl SilenceNotifier {
    /// A failed rule lookup costs the rule's channels, never the bound ones: a
    /// transient database error must not silence the whole notice.
    async fn channel_ids(&self, org: OrgId, alerts: &TargetAlerts, tags: &[String]) -> Vec<Uuid> {
        crate::storage::notification_channels::paging_channel_ids(
            self.channels.as_ref(),
            org,
            alerts,
            tags,
        )
        .await
        .unwrap_or_else(|_| alerts.iter().map(|b| b.channel_id).collect())
    }

    async fn deliver(&self, org: OrgId, channel_ids: Vec<Uuid>, notice: &IncidentNotice) -> bool {
        let central = self.central_bot.as_ref().map(|c| c.as_central());
        let mut delivered = false;
        for channel_id in channel_ids {
            let Ok(Some(channel)) = self.channels.get(org, channel_id).await else {
                continue;
            };
            if !channel.can_deliver() {
                continue;
            }
            let email_alert = crate::notifier::email_alert_for(
                self.orgs.as_ref(),
                &self.base_url,
                &self.alert_channel_stop_secret,
                org,
                &channel,
            )
            .await;
            let sent = match build_notifier(
                &channel.config,
                &self.http,
                central,
                self.central_whatsapp.as_ref(),
                self.email.as_ref(),
                email_alert,
                Default::default(),
            ) {
                Ok(n) => crate::notifier::notify_following_moves(
                    self.channels.as_ref(),
                    org,
                    &channel,
                    n.as_ref(),
                    notice,
                )
                .await
                .is_ok(),
                Err(_) => false,
            };
            // A landed send proves the endpoint is alive, whatever sent it.
            if sent
                && let Err(err) = self
                    .channels
                    .record_delivery_outcome(org, channel.id, true)
                    .await
            {
                tracing::warn!(channel_id = %channel.id, error = %err, "channel delivery run not cleared");
            }
            delivered |= sent;
        }
        delivered
    }
}

#[async_trait]
impl SilenceDelivery for SilenceNotifier {
    async fn notify(&self, org: OrgId, target_id: Uuid, reason: NotificationReason) -> bool {
        let Ok(Some(target)) = self.targets.get(org, target_id).await else {
            return false;
        };
        let channel_ids = self.channel_ids(org, &target.alerts, &target.tags).await;
        let notice = silence_notice(target_id, target.name, reason, None);
        self.deliver(org, channel_ids, &notice).await
    }

    async fn group_channels(&self, org: OrgId, group: &SilenceGroup) -> Vec<Uuid> {
        self.channel_ids(org, &group.alerts, &group.tags).await
    }

    async fn notify_group(&self, silence: &GroupSilence, reason: NotificationReason) -> bool {
        let notice = silence_notice(
            silence.id,
            silence.group_name.clone(),
            reason,
            Some(format!("Members: {}", member_list(&silence.members))),
        );
        self.deliver(silence.org, silence.channel_ids.clone(), &notice)
            .await
    }
}

pub async fn run(
    store: Arc<dyn SilenceStore>,
    delivery: Arc<dyn SilenceDelivery>,
    quotas: Arc<QuotaService>,
    stale_after: Duration,
    shutdown: CancellationToken,
) {
    let mut ticker = interval(TICK);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => return,
            _ = ticker.tick() => {
                match region_caps(store.as_ref(), &quotas).await {
                    // Resolved per tick, not held: a downgrade must reach the
                    // sweep on the same cadence it reaches the agent pull.
                    Ok(caps) => {
                        if let Err(err) =
                            sweep(store.as_ref(), Some(delivery.as_ref()), stale_after, &caps).await
                        {
                            tracing::warn!(error = %err, "silence sweep failed");
                        }
                    }
                    Err(err) => {
                        tracing::warn!(error = %err, "silence sweep skipped: region caps unresolved")
                    }
                }
            }
        }
    }
}

/// The ceiling the agent pull is applying right now, for the orgs this sweep
/// can see. Read from one place so silence and the pull cannot disagree about
/// which regions are actually probing a monitor.
async fn region_caps(store: &dyn SilenceStore, quotas: &QuotaService) -> Result<RegionCaps> {
    let orgs = store.orgs_with_assigned_targets().await?;
    Ok(RegionCaps::from(
        &crate::quotas::effective::resolve_plans(quotas, orgs).await,
    ))
}

/// One sweep: diff the live unmonitored set against recorded-open silences,
/// entering the newly silent and resolving the recovered, then notify customers
/// once per episode. `delivery` is `None` in tests that exercise only the state
/// diff. Visible for tests.
pub async fn sweep(
    store: &dyn SilenceStore,
    delivery: Option<&dyn SilenceDelivery>,
    stale_after: Duration,
    caps: &RegionCaps,
) -> Result<()> {
    let now = Utc::now();
    let unmonitored: HashSet<_> = store
        .unmonitored(stale_after.as_secs(), caps)
        .await?
        .into_iter()
        .collect();
    let open = store.list_open().await?;
    let open_ids: HashSet<_> = open.iter().map(|s| (s.org, s.target_id)).collect();

    for &(org, target_id) in unmonitored.difference(&open_ids) {
        store.enter(org, target_id, now).await?;
    }

    metrics::gauge!(metric_names::MONITORS_UNMONITORED).set(unmonitored.len() as f64);

    // A whole-region death is one infra event, not thousands of customer pages.
    let mass = if unmonitored.is_empty() {
        false
    } else {
        let total = store.enabled_target_count().await?.max(0);
        total > 0 && (unmonitored.len() as f64) > MAX_SILENCE_FRACTION * total as f64
    };

    for s in &open {
        if !unmonitored.contains(&(s.org, s.target_id)) {
            // Only the instance that flips the row notifies — no duplicate
            // resume. A member of a group's silence comes back inside it: the
            // group says so once, when every member has.
            if store.resolve(s.org, s.target_id, now).await? == Resolved::Alone
                && s.notified
                && !mass
                && let Some(d) = delivery
            {
                d.notify(s.org, s.target_id, NotificationReason::DataResumed)
                    .await;
            }
        }
    }

    if mass {
        tracing::warn!(
            unmonitored = unmonitored.len(),
            "many monitors unmonitored at once — likely a regional probe outage; customer notices suppressed"
        );
    } else if let Some(d) = delivery {
        notify_silent(store, d, now).await?;
    } else if !unmonitored.is_empty() {
        tracing::warn!(
            unmonitored = unmonitored.len(),
            "monitors have no live probe (silent)"
        );
    }
    if let Some(d) = delivery {
        settle_group_silences(store, d, now, mass).await?;
    }
    Ok(())
}

/// A silent monitor and since when, as a group silence holds it.
type SilentMember = (Uuid, chrono::DateTime<Utc>);

/// Tells the newly silent. A monitor whose group alerts as one goes into that
/// group's episode instead, which carries its notice: members of one group
/// silent in the same tick share one, and later ones join it quietly.
/// Send-then-mark keeps retrying a transient transport failure; a 2-instance
/// deploy overlap may double-send a no-data notice, which is benign.
async fn notify_silent(
    store: &dyn SilenceStore,
    d: &dyn SilenceDelivery,
    now: chrono::DateTime<Utc>,
) -> Result<()> {
    let pending: Vec<_> = store
        .list_open()
        .await?
        .into_iter()
        .filter(|s| !s.notified)
        .collect();
    let keys: Vec<(OrgId, Uuid)> = pending.iter().map(|s| (s.org, s.target_id)).collect();
    let groups = store.alerting_groups(&keys).await?;
    let mut by_group: HashMap<(OrgId, Uuid), (SilenceGroup, Vec<SilentMember>)> = HashMap::new();
    for s in pending {
        match groups.get(&(s.org, s.target_id)) {
            Some(group) => by_group
                .entry((s.org, group.id))
                .or_insert_with(|| (group.clone(), Vec::new()))
                .1
                .push((s.target_id, s.silent_since)),
            None => {
                if d.notify(s.org, s.target_id, NotificationReason::NoData)
                    .await
                {
                    store.mark_notified(s.org, s.target_id, now).await?;
                }
            }
        }
    }
    for ((org, _), (group, members)) in by_group {
        let channels = d.group_channels(org, &group).await;
        store
            .join_group_silence(org, &group, &channels, &members)
            .await?;
        for (target_id, _) in members {
            store.mark_notified(org, target_id, now).await?;
        }
    }
    Ok(())
}

/// Sends what group silence episodes owe: the opening notice until it lands,
/// and once every member is back the resumed one, which ends the episode. One
/// that never got its opening notice out ends without a word.
async fn settle_group_silences(
    store: &dyn SilenceStore,
    d: &dyn SilenceDelivery,
    now: chrono::DateTime<Utc>,
    mass: bool,
) -> Result<()> {
    for silence in store.group_silences_due().await? {
        if silence.finished {
            // Members all deleted: nothing came back to announce. A member
            // that joins after the read, which takes two sweeps at once, keeps
            // the episode open and the back notice repeats once it is back.
            let told = silence.notified
                && silence.resumed
                && !mass
                && d.notify_group(&silence, NotificationReason::DataResumed)
                    .await;
            store
                .end_group_silence(silence.org, silence.id, now, told)
                .await?;
        } else if !mass && d.notify_group(&silence, NotificationReason::NoData).await {
            store
                .mark_group_silence_notified(silence.org, silence.id, now)
                .await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::InMemorySilenceStore;
    use parking_lot::Mutex;

    #[derive(Default)]
    struct RecordingDelivery {
        sent: Mutex<Vec<(Uuid, NotificationReason)>>,
        group_sent: Mutex<Vec<(Uuid, NotificationReason, usize)>>,
    }
    #[async_trait]
    impl SilenceDelivery for RecordingDelivery {
        async fn notify(&self, _org: OrgId, target_id: Uuid, reason: NotificationReason) -> bool {
            self.sent.lock().push((target_id, reason));
            true
        }
        async fn group_channels(&self, _org: OrgId, _group: &SilenceGroup) -> Vec<Uuid> {
            Vec::new()
        }
        async fn notify_group(&self, silence: &GroupSilence, reason: NotificationReason) -> bool {
            self.group_sent
                .lock()
                .push((silence.id, reason, silence.members.len()));
            true
        }
    }

    fn group() -> SilenceGroup {
        SilenceGroup {
            id: Uuid::now_v7(),
            name: "DNS".into(),
            alerts: Default::default(),
            tags: Vec::new(),
        }
    }

    async fn tick(store: &InMemorySilenceStore, d: &RecordingDelivery) {
        sweep(
            store,
            Some(d),
            Duration::from_secs(120),
            &RegionCaps::default(),
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn a_group_alerting_as_one_says_once_that_its_members_went_silent_and_came_back() {
        let org = OrgId(Uuid::now_v7());
        let (ns, a, mx, alone) = (
            Uuid::now_v7(),
            Uuid::now_v7(),
            Uuid::now_v7(),
            Uuid::now_v7(),
        );
        let store = InMemorySilenceStore::new();
        store.set_total(100);
        let dns = group();
        for m in [ns, a, mx] {
            store.set_alerting_group(org, m, dns.clone());
        }
        let d = RecordingDelivery::default();

        store.set_unmonitored(vec![(org, ns), (org, a), (org, alone)]);
        tick(&store, &d).await;
        let opened = d.group_sent.lock().clone();
        assert_eq!(opened.len(), 1);
        assert_eq!((opened[0].1, opened[0].2), (NotificationReason::NoData, 2));
        assert_eq!(*d.sent.lock(), vec![(alone, NotificationReason::NoData)]);

        // A later member joins without a notice; one back alone says nothing.
        store.set_unmonitored(vec![(org, ns), (org, a), (org, mx)]);
        tick(&store, &d).await;
        store.set_unmonitored(vec![(org, a), (org, mx)]);
        tick(&store, &d).await;
        assert_eq!(d.group_sent.lock().len(), 1);

        store.set_unmonitored(vec![]);
        tick(&store, &d).await;
        let all = d.group_sent.lock().clone();
        assert_eq!(all.len(), 2);
        assert_eq!(all[1], (opened[0].0, NotificationReason::DataResumed, 3));
        assert_eq!(
            count(&d, NotificationReason::DataResumed),
            1,
            "the lone monitor only"
        );
    }

    #[tokio::test]
    async fn a_mass_outage_tells_no_group_either() {
        let org = OrgId(Uuid::now_v7());
        let store = InMemorySilenceStore::new();
        store.set_total(4);
        let dns = group();
        let silent: Vec<_> = (0..3).map(|_| (org, Uuid::now_v7())).collect();
        for (o, t) in &silent {
            store.set_alerting_group(*o, *t, dns.clone());
        }
        let d = RecordingDelivery::default();
        store.set_unmonitored(silent);
        tick(&store, &d).await;
        assert!(d.group_sent.lock().is_empty());
        assert!(d.sent.lock().is_empty());
    }

    fn count(d: &RecordingDelivery, reason: NotificationReason) -> usize {
        d.sent
            .lock()
            .iter()
            .filter(|(_, r)| std::mem::discriminant(r) == std::mem::discriminant(&reason))
            .count()
    }

    #[tokio::test]
    async fn enters_new_and_resolves_recovered() {
        let org = OrgId(Uuid::now_v7());
        let a = Uuid::now_v7();
        let b = Uuid::now_v7();
        let store = InMemorySilenceStore::new();

        store.set_unmonitored(vec![(org, a), (org, b)]);
        sweep(
            &store,
            None,
            Duration::from_secs(120),
            &RegionCaps::default(),
        )
        .await
        .unwrap();
        assert_eq!(store.open_target_ids(), sorted(vec![a, b]));

        store.set_unmonitored(vec![(org, b)]);
        sweep(
            &store,
            None,
            Duration::from_secs(120),
            &RegionCaps::default(),
        )
        .await
        .unwrap();
        assert_eq!(store.open_target_ids(), vec![b]);
        assert_eq!(store.resolved_target_ids(), vec![a]);
    }

    #[tokio::test]
    async fn notifies_once_then_resumes() {
        let org = OrgId(Uuid::now_v7());
        let a = Uuid::now_v7();
        let store = InMemorySilenceStore::new();
        store.set_total(10);
        let d = RecordingDelivery::default();

        // Silent across two sweeps → exactly one NoData notice.
        store.set_unmonitored(vec![(org, a)]);
        sweep(
            &store,
            Some(&d),
            Duration::from_secs(120),
            &RegionCaps::default(),
        )
        .await
        .unwrap();
        sweep(
            &store,
            Some(&d),
            Duration::from_secs(120),
            &RegionCaps::default(),
        )
        .await
        .unwrap();
        assert_eq!(count(&d, NotificationReason::NoData), 1);

        // Recovery → one DataResumed (it had been notified).
        store.set_unmonitored(vec![]);
        sweep(
            &store,
            Some(&d),
            Duration::from_secs(120),
            &RegionCaps::default(),
        )
        .await
        .unwrap();
        assert_eq!(count(&d, NotificationReason::DataResumed), 1);
    }

    #[tokio::test]
    async fn mass_outage_suppresses_customer_notices() {
        let org = OrgId(Uuid::now_v7());
        let store = InMemorySilenceStore::new();
        store.set_total(10);
        let d = RecordingDelivery::default();
        // 6 of 10 silent (>25%) → no customer notices, state still recorded.
        let silent: Vec<_> = (0..6).map(|_| (org, Uuid::now_v7())).collect();
        store.set_unmonitored(silent.clone());
        sweep(
            &store,
            Some(&d),
            Duration::from_secs(120),
            &RegionCaps::default(),
        )
        .await
        .unwrap();
        assert_eq!(count(&d, NotificationReason::NoData), 0);
        assert_eq!(store.open_target_ids().len(), 6);
    }

    fn sorted(mut v: Vec<Uuid>) -> Vec<Uuid> {
        v.sort();
        v
    }
}
