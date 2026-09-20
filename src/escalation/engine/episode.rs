use chrono::Utc;
use futures::stream::{FuturesUnordered, StreamExt};
use tokio::time::Instant;
use uuid::Uuid;

use crate::domain::{
    ChannelConfig, EscalationDecision, IncidentEventKind, NotificationReason, OpsIncident, OrgId,
    Target, next_step,
};
use crate::error::Result;
use crate::notifier::pushover::PushoverReceipts;
use crate::storage::{Actor, DueIncident, EmergencyAck, LifecycleOutcome};

use super::rules::{
    DAMPED_TRANSPORT, Damper, FlapState, MAINTENANCE_TRANSPORT, RELEASED_TRANSPORT,
    UNREACHABLE_TRANSPORT, channel_targets, flap_state, open_episode_active, resolvable_channels,
};
use super::{SWEEP_CONCURRENCY, Worker};

impl Worker {
    /// Reconcile dropped open signals, then walk due escalations and retry
    /// failed pages. Runs on a detached task off the rx loop.
    pub(super) async fn sweep(&self) {
        self.reconcile().await;
        self.release_held().await;
        self.release_maintenance().await;
        self.escalate_due().await;
        self.renotify_due().await;
        self.retry_pending().await;
        self.poll_acks().await;
    }

    /// Catch incidents whose `Opened` signal was lost (e.g. the bounded signal
    /// channel saturated during a correlated mass outage): a `triggered`
    /// incident, older than the grace window, that was never paged and never
    /// armed. Re-running `page(Opened)` is idempotent — `open_episode` no-ops if
    /// the episode is already active. The DB is the source of truth, not the
    /// in-memory channel. Bounded on both sides: a monitor with no channel
    /// bound records nothing, so it would otherwise match this scan forever.
    pub(super) async fn reconcile(&self) {
        let limit = self.cfg.max_pages_per_tick.max(1) as usize;
        let now = Utc::now();
        let grace = chrono::Duration::seconds(self.cfg.tick_interval_secs.max(1) as i64);
        let window = chrono::Duration::seconds(self.cfg.reconcile_window_secs.max(1) as i64);
        let due = match self
            .ops
            .due_for_reconcile((now - window, now - grace), limit)
            .await
        {
            Ok(d) => d,
            Err(err) => {
                tracing::warn!(error = %err, "escalation reconcile scan failed");
                return;
            }
        };
        if !due.is_empty() {
            tracing::warn!(
                count = due.len(),
                "reconciling incidents that were never paged"
            );
        }
        let budget = self.sweep_budget();
        let start = Instant::now();
        let mut it = due.into_iter();
        let mut futs = FuturesUnordered::new();
        for d in it.by_ref().take(SWEEP_CONCURRENCY) {
            futs.push(self.reconcile_one_logged(d));
        }
        while futs.next().await.is_some() {
            if start.elapsed() < budget
                && let Some(d) = it.next()
            {
                futs.push(self.reconcile_one_logged(d));
            }
        }
    }

    /// Page held alerts whose incident is still open past the hold — a flap
    /// closes well inside it, so anything left is a real outage. Runs with
    /// damping off too, or switching it off would strand existing holds.
    ///
    /// Unleased, unlike [`escalate_due`](Self::escalate_due): nothing is
    /// claimed at scan time and the row that closes the predicate is only
    /// written mid-page, so two replicas would each release the same hold.
    /// BLOCKER for running more than one control plane — take a lease here
    /// first, as `escalate_due` does.
    pub(super) async fn release_held(&self) {
        let limit = self.cfg.max_pages_per_tick.max(1) as usize;
        let hold = chrono::Duration::seconds(self.cfg.flap_hold_secs.max(1) as i64);
        let due = match self.ops.due_for_flap_release(Utc::now(), hold, limit).await {
            Ok(d) => d,
            Err(err) => {
                tracing::warn!(error = %err, "flap release scan failed");
                return;
            }
        };
        if !due.is_empty() {
            tracing::info!(
                count = due.len(),
                "paging held alerts whose incident is still open"
            );
        }
        let budget = self.sweep_budget();
        let start = Instant::now();
        let mut it = due.into_iter();
        let mut futs = FuturesUnordered::new();
        for d in it.by_ref().take(SWEEP_CONCURRENCY) {
            futs.push(self.release_one_logged(d));
        }
        while futs.next().await.is_some() {
            if start.elapsed() < budget
                && let Some(d) = it.next()
            {
                futs.push(self.release_one_logged(d));
            }
        }
    }

    /// Page alerts a maintenance window held, once it lets go. Unleased, with
    /// the same multi-replica caveat as [`release_held`](Self::release_held).
    pub(super) async fn release_maintenance(&self) {
        let limit = self.cfg.max_pages_per_tick.max(1) as usize;
        let due = match self.ops.due_for_maintenance_release(limit).await {
            Ok(d) => d,
            Err(err) => {
                tracing::warn!(error = %err, "maintenance release scan failed");
                return;
            }
        };
        if !due.is_empty() {
            tracing::info!(
                count = due.len(),
                "paging alerts held by a maintenance window that has ended"
            );
        }
        let budget = self.sweep_budget();
        let start = Instant::now();
        let mut it = due.into_iter();
        let mut futs = FuturesUnordered::new();
        for d in it.by_ref().take(SWEEP_CONCURRENCY) {
            futs.push(self.release_one_logged(d));
        }
        while futs.next().await.is_some() {
            if start.elapsed() < budget
                && let Some(d) = it.next()
            {
                futs.push(self.release_one_logged(d));
            }
        }
    }

    async fn release_one_logged(&self, d: DueIncident) {
        if let Err(err) = self.release_one(&d).await {
            tracing::warn!(incident_id = %d.id, error = %err, "held alert release failed");
        }
    }

    /// A release that reaches no channel records nothing, and the scan keys
    /// off "nothing newer than the hold", so it would re-release every tick
    /// forever. The marker closes that. Written after the attempt, so a crash
    /// retries rather than swallowing the page.
    async fn release_one(&self, d: &DueIncident) -> Result<()> {
        let before: Vec<Uuid> = self
            .ops
            .notifications_for(d.org, d.id)
            .await?
            .iter()
            .map(|n| n.id)
            .collect();
        let reason = self.held_reason(d.org, d.id).await;
        self.page_with(d.org, d.id, reason, Damper::Skip).await?;
        // By row id, not timestamp: `created_at` is the database's clock and
        // ours is this process's, so skew would misreport a delivered release.
        let paged = self
            .ops
            .notifications_for(d.org, d.id)
            .await?
            .iter()
            .any(|n| !before.contains(&n.id));
        if paged {
            return Ok(());
        }
        self.ops
            .record_notification(crate::domain::NewIncidentNotification {
                org: d.org,
                incident_id: d.id,
                escalation_level: None,
                target_user_id: None,
                channel_id: None,
                transport: RELEASED_TRANSPORT.to_string(),
                reason,
                status: crate::domain::NotificationStatus::Suppressed,
                attempt: 0,
                error: None,
                sent_at: None,
            })
            .await?;
        Ok(())
    }

    /// The reason the held row recorded, so a released hold pages as the
    /// reopen it was rather than as a fresh outage.
    async fn held_reason(&self, org: OrgId, incident_id: Uuid) -> NotificationReason {
        self.ops
            .notifications_for(org, incident_id)
            .await
            .ok()
            .and_then(|rows| {
                rows.iter()
                    .filter(|n| {
                        n.transport == DAMPED_TRANSPORT || n.transport == MAINTENANCE_TRANSPORT
                    })
                    .max_by_key(|n| n.created_at)
                    .map(|n| n.reason)
            })
            .unwrap_or(NotificationReason::Opened)
    }

    async fn reconcile_one_logged(&self, d: DueIncident) {
        if let Err(err) = self.page(d.org, d.id, NotificationReason::Opened).await {
            tracing::warn!(incident_id = %d.id, error = %err, "incident reconcile page failed");
        }
    }

    /// Handle a lifecycle signal. Opened/Reopened start the escalation episode
    /// (page the first rung, arm the timer); Resolved notifies the channels
    /// already paged this episode. The escalation sweep handles later rungs.
    pub(super) async fn page(
        &self,
        org: OrgId,
        incident_id: Uuid,
        reason: NotificationReason,
    ) -> Result<()> {
        self.page_with(org, incident_id, reason, Damper::Apply)
            .await
    }

    /// `Damper::Skip` is the deferred-release path: the hold has already
    /// served as the filter, so re-damping would silence it forever.
    pub(super) async fn page_with(
        &self,
        org: OrgId,
        incident_id: Uuid,
        reason: NotificationReason,
        damper: Damper,
    ) -> Result<()> {
        // Serialise all paging for this incident: the dedup in open_episode /
        // notify_resolution is read-then-act, so without this a concurrent
        // signal task + sweep (reconcile) task could both open the same episode
        // and double-page. Held for the whole resolve+page+record sequence.
        let _guard = self.page_lock(incident_id).lock().await;
        let Some(incident) = self.ops.get(org, incident_id).await? else {
            return Ok(());
        };
        let Some(target_id) = incident.target_id else {
            return Ok(());
        };
        let Some(target) = self.targets.get(org, target_id).await? else {
            return Ok(());
        };
        match reason {
            // A held monitor rides the pause branch: the writer already skips
            // it, so this only catches an episode that was open when the hold
            // landed. Resolution still goes out, or an outage that ended would
            // stay on the customer's timeline forever.
            _ if (!target.enabled || target.plan_hold_at.is_some())
                && reason != NotificationReason::Resolved =>
            {
                Ok(())
            }
            NotificationReason::Opened | NotificationReason::Reopened => {
                // Retire the last episode's pages before the new one starts.
                if reason == NotificationReason::Reopened {
                    self.cancel_emergency(org, incident.id).await;
                }
                self.open_episode(org, &incident, &target, reason, damper)
                    .await
            }
            NotificationReason::Resolved => self.notify_resolution(org, &incident, &target).await,
            // Escalation and reminder pages originate from the sweep, never an
            // inbound signal.
            NotificationReason::Escalated | NotificationReason::Reminder => Ok(()),
            // Silence has no incident row; the silence sweep delivers it.
            NotificationReason::NoData | NotificationReason::DataResumed => Ok(()),
        }
    }

    /// Page the first rung and arm escalation. A duplicate Opened signal while
    /// the episode is already paged is a no-op; a monitor with no policy falls
    /// back to its bound channels (the pre-policy behaviour) with no laddered
    /// re-paging.
    async fn open_episode(
        &self,
        org: OrgId,
        incident: &OpsIncident,
        target: &Target,
        reason: NotificationReason,
        damper: Damper,
    ) -> Result<()> {
        let already = self.ops.notifications_for(org, incident.id).await?;
        if open_episode_active(&already) {
            return Ok(());
        }
        // The scan read `triggered` a sweep ago and a flapping monitor
        // recovers inside that gap. Its all-clear already ran and reached
        // nobody, so paging now announces an outage no recovery would follow.
        // Here because `page` holds the per-incident lock.
        if damper == Damper::Skip && incident.state != crate::domain::IncidentState::Triggered {
            return Ok(());
        }
        let mut note = None;
        // A hand-declared incident is the operator's own signal: the count
        // already excludes them, and holding one would silence a real outage
        // someone declared deliberately. A maintenance window is the same
        // bargain, so it exempts them too.
        let operator_declared = incident.origin == crate::domain::IncidentOrigin::Manual;
        if !operator_declared && self.alerts_suppressed(org, target.id).await {
            return self.hold_for_maintenance(org, incident.id, reason).await;
        }
        if damper == Damper::Apply && !operator_declared {
            match self.flap_state(org, target).await? {
                FlapState::Steady => {}
                FlapState::Crossing => {
                    self.note_flap_engaged(org, incident.id, target).await?;
                    note = Some(self.flap_notice());
                }
                FlapState::Damped => return self.hold(org, incident.id, reason).await,
            }
        }
        let notice = self.notice(incident, target, reason, note);
        match self.policies.resolve_for_target(org, target.id).await? {
            Some(policy_id) => {
                let Some(policy) = self.policies.get(org, policy_id).await? else {
                    return Ok(());
                };
                match next_step(&policy.steps, policy.repeat_count, 0, 0) {
                    EscalationDecision::Page {
                        level, delay_secs, ..
                    } => {
                        let targets = self
                            .resolve_targets(org, &policy, level, Utc::now())
                            .await?;
                        if targets.is_empty() {
                            self.note_empty_rung(org, incident.id, level).await?;
                        }
                        let paged = self
                            .page_channels(org, incident.id, &notice, reason, level, &targets)
                            .await?;
                        let next_at =
                            Some(Utc::now() + chrono::Duration::seconds(delay_secs.into()));
                        self.ops
                            .begin_escalation(org, incident.id, policy_id, level, next_at)
                            .await?;
                        self.log_paged(org, incident.id, reason, paged.delivered)
                            .await?;
                    }
                    EscalationDecision::Exhausted => {
                        // Policy with no steps: record the binding so the
                        // console shows it, but page no one.
                        self.ops
                            .begin_escalation(org, incident.id, policy_id, 0, None)
                            .await?;
                    }
                }
            }
            None => {
                // Propagated on purpose: nothing is written, so the reconcile
                // scan retries this episode whole. Falling back to the bound
                // channels would record rows, and a recorded episode is one
                // that scan never revisits — the rule's channels would then
                // miss the outage entirely rather than be paged a tick late.
                let targets = channel_targets(
                    crate::storage::notification_channels::paging_channel_ids(
                        self.channels.as_ref(),
                        org,
                        target,
                    )
                    .await?,
                );
                let paged = self
                    .page_channels(org, incident.id, &notice, reason, 0, &targets)
                    .await?;
                self.log_paged(org, incident.id, reason, paged.delivered)
                    .await?;
                if paged.recorded == 0 {
                    // Nothing bound, or every bound channel gone: without a row
                    // the reconcile scan re-runs this episode every tick for the
                    // whole window, re-appending its timeline note each time.
                    self.record_unreachable(org, incident.id, reason).await?;
                }
            }
        }
        Ok(())
    }

    /// A lookup failure degrades to [`FlapState::Steady`]: failing to read the
    /// history must not silence a real outage.
    async fn flap_state(&self, org: OrgId, target: &Target) -> Result<FlapState> {
        let max = self.cfg.flap_max_opens;
        if max == 0 {
            return Ok(FlapState::Steady);
        }
        let since = Utc::now() - chrono::Duration::seconds(self.cfg.flap_window_secs.max(1) as i64);
        let opens = match self.ops.opens_since(org, target.id, since).await {
            Ok(n) => n,
            Err(err) => {
                tracing::warn!(target_id = %target.id, error = %err, "flap lookup failed; paging normally");
                return Ok(FlapState::Steady);
            }
        };
        Ok(flap_state(opens, max))
    }

    /// What the crossing alert tells the recipient, so alerts going quiet is
    /// never a surprise.
    fn flap_notice(&self) -> String {
        format!(
            "Flapping: {} failures in {}m, further alerts held unless an outage lasts {}m.",
            self.cfg.flap_max_opens,
            self.cfg.flap_window_secs.div_ceil(60),
            self.cfg.flap_hold_secs.div_ceil(60),
        )
    }

    /// Without this the operator just sees alerts stop.
    async fn note_flap_engaged(
        &self,
        org: OrgId,
        incident_id: Uuid,
        target: &Target,
    ) -> Result<()> {
        self.ops
            .append_event(
                org,
                incident_id,
                IncidentEventKind::Notified,
                Actor::System,
                Some(format!(
                    "\"{}\" has opened {} incidents within {} minutes — alerts for further \
                     opens are held until it settles",
                    target.name,
                    self.cfg.flap_max_opens,
                    self.cfg.flap_window_secs.div_ceil(60)
                )),
            )
            .await
    }

    /// Mark that an episode reached no channel, so the scans that key off
    /// "has this been paged" stop re-running it.
    async fn record_unreachable(
        &self,
        org: OrgId,
        incident_id: Uuid,
        reason: NotificationReason,
    ) -> Result<()> {
        self.ops
            .record_notification(crate::domain::NewIncidentNotification {
                org,
                incident_id,
                escalation_level: None,
                target_user_id: None,
                channel_id: None,
                transport: UNREACHABLE_TRANSPORT.to_string(),
                reason,
                status: crate::domain::NotificationStatus::Suppressed,
                attempt: 0,
                error: None,
                sent_at: None,
            })
            .await
            .map(|_| ())
    }

    /// Hold this open rather than deliver it. The row is what the release scan
    /// keys off, so a hold is never a drop, and it also keeps the reconcile
    /// scan from re-running the episode every tick.
    async fn hold(&self, org: OrgId, incident_id: Uuid, reason: NotificationReason) -> Result<()> {
        metrics::counter!(crate::metric_names::ALERTS_DAMPED).increment(1);
        self.ops
            .record_notification(crate::domain::NewIncidentNotification {
                org,
                incident_id,
                escalation_level: None,
                target_user_id: None,
                channel_id: None,
                transport: DAMPED_TRANSPORT.to_string(),
                reason,
                status: crate::domain::NotificationStatus::Suppressed,
                attempt: 0,
                error: None,
                sent_at: None,
            })
            .await?;
        self.ops
            .append_event(
                org,
                incident_id,
                IncidentEventKind::Note,
                Actor::System,
                Some(format!(
                    "alert held: the monitor is flapping. It pages anyway if this \
                     incident is still open in {} minutes",
                    self.cfg.flap_hold_secs.div_ceil(60)
                )),
            )
            .await
    }

    /// Send the all-clear to every channel paged this episode that has not
    /// already had one, honouring a binding's recovery opt-out.
    async fn notify_resolution(
        &self,
        org: OrgId,
        incident: &OpsIncident,
        target: &Target,
    ) -> Result<()> {
        // Stop any still-repeating emergency page before the recovery-notice
        // gate — a resolved incident must go quiet even when the recovery push
        // itself is disabled.
        self.cancel_emergency(org, incident.id).await;
        if !target.notify_recovery {
            return Ok(());
        }
        let rows = self.ops.notifications_for(org, incident.id).await?;
        let channels: Vec<Uuid> = resolvable_channels(&rows);
        if channels.is_empty() {
            return Ok(());
        }
        let notice = self.notice(incident, target, NotificationReason::Resolved, None);
        let paged = self
            .page_channels(
                org,
                incident.id,
                &notice,
                NotificationReason::Resolved,
                incident.escalation_level,
                &channel_targets(channels),
            )
            .await?;
        self.log_paged(
            org,
            incident.id,
            NotificationReason::Resolved,
            paged.delivered,
        )
        .await
    }

    /// Build a Pushover receipt client from the channel's stored application
    /// token. `None` if the channel is gone or not a Pushover channel.
    /// `Err` means the lookup failed and the caller must not conclude anything
    /// about the channel; `Ok(None)` means it is genuinely gone or is not a
    /// Pushover channel, and its receipt can never be cancelled or read.
    async fn pushover_receipts(
        &self,
        org: OrgId,
        channel_id: Uuid,
    ) -> Result<Option<PushoverReceipts>> {
        Ok(match self.channels.get(org, channel_id).await? {
            Some(channel) => match &channel.config {
                ChannelConfig::Pushover(c) => {
                    Some(PushoverReceipts::new(self.http.clone(), c.token.clone()))
                }
                _ => None,
            },
            None => None,
        })
    }

    /// Stop every emergency page still repeating for an incident. A receipt is
    /// retired only once its cancel lands, or a failure would leave the page
    /// sounding with nothing left to retry it; Pushover's `expire` bounds one
    /// that never lands.
    async fn cancel_emergency(&self, org: OrgId, incident_id: Uuid) {
        let acks = match self.ops.emergency_acks_for_incident(org, incident_id).await {
            Ok(a) => a,
            Err(err) => {
                tracing::warn!(error = %err, "emergency ack lookup failed");
                return;
            }
        };
        for ack in acks {
            let client = match self.pushover_receipts(org, ack.channel_id).await {
                // Nothing can cancel it — retire the receipt.
                Ok(None) => {
                    let _ = self.ops.clear_receipt(org, ack.id).await;
                    continue;
                }
                Ok(Some(client)) => client,
                // Leave it for the next sweep: retiring a receipt we could not
                // even look up would strand a page that keeps sounding.
                Err(err) => {
                    tracing::warn!(error = %err, "pushover channel lookup failed");
                    continue;
                }
            };
            if let Err(err) = client.cancel(&ack.receipt).await {
                tracing::warn!(incident_id = %incident_id, error = %err, "pushover emergency cancel failed");
                continue;
            }
            if let Err(err) = self.ops.clear_receipt(org, ack.id).await {
                tracing::warn!(error = %err, "clearing emergency receipt failed");
            }
        }
    }

    /// A lookup failure reads as no maintenance, so an error never silences a page.
    pub(super) async fn alerts_suppressed(&self, org: OrgId, target_id: Uuid) -> bool {
        matches!(
            self.maintenance.alerts_suppressed(org, target_id).await,
            Ok(true)
        )
    }

    /// The marker row is what the release scan keys off, so a hold is never a
    /// drop, and it stops the reconcile scan re-running the episode every tick.
    async fn hold_for_maintenance(
        &self,
        org: OrgId,
        incident_id: Uuid,
        reason: NotificationReason,
    ) -> Result<()> {
        self.ops
            .record_notification(crate::domain::NewIncidentNotification {
                org,
                incident_id,
                escalation_level: None,
                target_user_id: None,
                channel_id: None,
                transport: MAINTENANCE_TRANSPORT.to_string(),
                reason,
                status: crate::domain::NotificationStatus::Suppressed,
                attempt: 0,
                error: None,
                sent_at: None,
            })
            .await?;
        self.ops
            .append_event(
                org,
                incident_id,
                IncidentEventKind::Note,
                Actor::System,
                Some(
                    "alert held: the monitor is in a maintenance window. It pages \
                     when the window ends if this incident is still open"
                        .to_string(),
                ),
            )
            .await?;
        metrics::counter!(crate::metric_names::ALERTS_HELD_MAINTENANCE).increment(1);
        Ok(())
    }

    /// Nothing is left for this page to acknowledge: its incident was taken by
    /// somebody, resolved, or removed, its outage ended and another has since
    /// started, or its monitor is no longer watched. One incident read serves
    /// all three. Any failed lookup reads as still paging, so an error never
    /// cancels a live page.
    pub(super) async fn page_is_spent(&self, ack: &EmergencyAck) -> bool {
        let incident = match self.ops.get(ack.org, ack.incident_id).await {
            Ok(Some(incident)) => incident,
            Ok(None) => return true,
            Err(_) => return false,
        };
        if incident.state != crate::domain::IncidentState::Triggered {
            return true;
        }
        if matches!(
            self.ops.generation(ack.org, ack.incident_id).await,
            Ok(Some(current)) if current != ack.generation
        ) {
            return true;
        }
        let Some(target_id) = incident.target_id else {
            return false;
        };
        matches!(self.targets.get(ack.org, target_id).await,
            Ok(Some(t)) if !t.enabled || t.plan_hold_at.is_some())
    }

    /// Poll outstanding emergency receipts: record acknowledgement on the
    /// timeline, drop the receipt once acked or expired so it leaves the poll
    /// set.
    async fn poll_acks(&self) {
        let limit = self.cfg.max_pages_per_tick.max(1) as usize;
        let due = match self.ops.due_emergency_acks(limit).await {
            Ok(d) => d,
            Err(err) => {
                tracing::warn!(error = %err, "emergency ack poll scan failed");
                return;
            }
        };
        let budget = self.sweep_budget();
        let start = Instant::now();
        let mut it = due.into_iter();
        let mut futs = FuturesUnordered::new();
        for a in it.by_ref().take(SWEEP_CONCURRENCY) {
            futs.push(self.poll_one_ack(a));
        }
        while futs.next().await.is_some() {
            if start.elapsed() < budget
                && let Some(a) = it.next()
            {
                futs.push(self.poll_one_ack(a));
            }
        }
    }

    async fn poll_one_ack(&self, ack: EmergencyAck) {
        let client = match self.pushover_receipts(ack.org, ack.channel_id).await {
            // Nothing can cancel or read it — retire the receipt.
            Ok(None) => {
                let _ = self.ops.clear_receipt(ack.org, ack.id).await;
                return;
            }
            Ok(Some(client)) => client,
            Err(err) => {
                tracing::warn!(error = %err, "pushover channel lookup failed");
                return;
            }
        };
        let state = match client.poll(&ack.receipt).await {
            Ok(s) => s,
            Err(err) => {
                tracing::warn!(error = %err, "pushover receipt poll failed");
                return;
            }
        };
        if state.acknowledged {
            // Pushover stops its own retries on an ack, but until this landed
            // nothing here knew the page was taken, so renotify kept paging.
            match self
                .ops
                .acknowledge(
                    ack.org,
                    ack.incident_id,
                    Actor::Link,
                    Some("Acknowledged in Pushover".to_string()),
                    // A reopen tries to cancel this receipt, but the signal
                    // can be dropped under load and the cancel can fail, so
                    // the episode is pinned rather than assumed.
                    Some(ack.generation),
                )
                .await
            {
                Ok(LifecycleOutcome::Updated(_)) => {
                    // Marked first, so the sweep below skips this row.
                    let _ = self.ops.mark_acked(ack.org, ack.id, Utc::now()).await;
                    self.cancel_emergency(ack.org, ack.incident_id).await;
                }
                // Terminal: no later poll does better.
                Ok(
                    LifecycleOutcome::NotFound
                    | LifecycleOutcome::IllegalTransition(_)
                    | LifecycleOutcome::Stale,
                ) => {
                    let _ = self.ops.mark_acked(ack.org, ack.id, Utc::now()).await;
                }
                // Unmarked, so the next sweep retries; marking it here would
                // lose the acknowledgement for good.
                Err(err) => {
                    tracing::warn!(error = %err, "recording emergency ack failed");
                }
            }
        } else if state.expired {
            let _ = self.ops.clear_receipt(ack.org, ack.id).await;
        } else if self.page_is_spent(&ack).await {
            // Nothing should still be sounding for it. Retired only once the
            // cancel lands, so a failure comes back on the next sweep.
            if let Err(err) = client.cancel(&ack.receipt).await {
                tracing::warn!(incident_id = %ack.incident_id, error = %err, "pushover emergency cancel failed");
                return;
            }
            let _ = self.ops.clear_receipt(ack.org, ack.id).await;
        }
    }
}
