use chrono::Utc;
use futures::stream::{FuturesUnordered, StreamExt};
use tokio::time::Instant;
use uuid::Uuid;

use crate::domain::{
    EscalationDecision, IncidentEventKind, IncidentState, NotificationReason, NotificationStatus,
    OpsIncident, OrgId, Target, next_step, wait_after,
};
use crate::error::Result;
use crate::storage::{Arming, ClosingNotice, DueIncident, FanOutKind, OwedPage};

use super::rules::{
    DAMPED_TRANSPORT, Damper, FlapState, MAINTENANCE_TRANSPORT, RELEASED_TRANSPORT,
    UNREACHABLE_TRANSPORT, channel_targets, flap_state, open_episode_active, opening_reason,
    resolvable_channels,
};
use super::sweep::{Planned, empty_rung_event};
use super::{SWEEP_CONCURRENCY, Worker};

impl Worker {
    /// Page what a dropped signal left owed, open and closing, then walk due
    /// escalations and retry failed pages. Runs on a detached task off the rx
    /// loop.
    pub(super) async fn sweep(&self) {
        self.page_owed().await;
        self.reconcile_closed().await;
        self.release_held().await;
        self.release_maintenance().await;
        self.escalate_due().await;
        self.renotify_due().await;
        self.retry_pending().await;
        self.poll_acks().await;
    }

    /// Page every episode still owing its first page a tick after it began,
    /// whatever its age: the signal for it was dropped (a restart, or the
    /// bounded signal channel full under a mass outage), or its monitor was
    /// paused or held when it came and is watched again. The database says
    /// what is owed, not the in-memory channel, and recording the page settles
    /// it, so a late signal for the same episode finds nothing to do.
    pub(super) async fn page_owed(&self) {
        let limit = self.cfg.max_pages_per_tick.max(1) as usize;
        let grace = chrono::Duration::seconds(self.cfg.tick_interval_secs.max(1) as i64);
        let due = match self.ops.due_for_owed_page(Utc::now() - grace, limit).await {
            Ok(d) => d,
            Err(err) => {
                tracing::warn!(error = %err, "owed page scan failed");
                return;
            }
        };
        if !due.is_empty() {
            tracing::warn!(
                count = due.len(),
                "paging incidents whose first page never went out"
            );
        }
        let budget = self.sweep_budget();
        let start = Instant::now();
        let mut it = due.into_iter();
        let mut futs = FuturesUnordered::new();
        for o in it.by_ref().take(SWEEP_CONCURRENCY) {
            futs.push(self.page_owed_one(o));
        }
        while futs.next().await.is_some() {
            if start.elapsed() < budget
                && let Some(o) = it.next()
            {
                futs.push(self.page_owed_one(o));
            }
        }
    }

    /// Each one tried moves behind the rest, whatever came of it, so one that
    /// keeps failing never holds up the pages behind it.
    async fn page_owed_one(&self, o: OwedPage) {
        // Named opened or reopened by the episode it lands in.
        if let Err(err) = self.page(o.org, o.id, NotificationReason::Opened).await {
            tracing::warn!(incident_id = %o.id, error = %err, "owed page failed");
        }
        if let Err(err) = self.ops.requeue_owed_page(o.org, o.id, o.owed_at).await {
            tracing::warn!(incident_id = %o.id, error = %err, "owed page not requeued");
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

    /// Send the closing notices whose signal never arrived: a restart between
    /// the close and its signal, or a signal channel full under a mass outage.
    /// The close recorded the notice as owed in the statement that ended the
    /// incident, so the database says what is missing. The signal gets a tick
    /// to land first. Whichever of the two claims a notice sends it, so they
    /// never both do, even from two engine instances.
    pub(super) async fn reconcile_closed(&self) {
        let limit = self.cfg.max_pages_per_tick.max(1) as usize;
        let grace = self.cfg.tick_interval_secs.max(1) as i64;
        let window = self.cfg.closing_window_secs.max(1) as i64;
        let due = match self
            .ops
            .due_closing_notices(grace, window, self.claim_lease_secs(), limit)
            .await
        {
            Ok(d) => d,
            Err(err) => {
                tracing::warn!(error = %err, "closing notice scan failed");
                return;
            }
        };
        if due.expired > 0 {
            tracing::warn!(
                count = due.expired,
                "closing notices withdrawn unsent: their incident ended outside the window"
            );
        }
        if !due.claimed.is_empty() {
            tracing::warn!(
                count = due.claimed.len(),
                "sending closing notices whose signal never arrived"
            );
        }
        let budget = self.sweep_budget();
        let start = Instant::now();
        let mut it = due.claimed.into_iter();
        let mut futs = FuturesUnordered::new();
        for n in it.by_ref().take(SWEEP_CONCURRENCY) {
            futs.push(self.close_one_logged(n));
        }
        while futs.next().await.is_some() {
            if start.elapsed() < budget
                && let Some(n) = it.next()
            {
                futs.push(self.close_one_logged(n));
            }
        }
    }

    async fn close_one_logged(&self, n: ClosingNotice) {
        let _guard = self.page_lock(n.id).lock().await;
        if let Err(err) = self.close_out(n.org, n.id, n.claim).await {
            tracing::warn!(incident_id = %n.id, error = %err, "closing notice failed");
        }
    }

    async fn release_one_logged(&self, d: DueIncident) {
        if let Err(err) = self.release_one(&d).await {
            tracing::warn!(incident_id = %d.id, error = %err, "held alert release failed");
        }
    }

    /// A release that reaches no channel ends its hold with the `released`
    /// marker, inside its own page, so the scan's "nothing newer than the
    /// hold" stops matching. A monitor paused or held since the scan pages
    /// nothing and records nothing: the hold waits for it to come back.
    async fn release_one(&self, d: &DueIncident) -> Result<()> {
        // Named opened or reopened by the episode it lands in.
        self.page_with(d.org, d.id, NotificationReason::Opened, Damper::Skip)
            .await
    }

    /// Handle a lifecycle signal. Opened/Reopened start the escalation episode
    /// (page the first rung, arm the timer); Resolved/MonitorDeleted send the
    /// notice the close owes to the channels already paged this episode. The
    /// escalation sweep handles later rungs.
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
        // Serialise all paging for this incident in this process, so a signal
        // and a sweep never interleave their reads and sends; which page
        // records is the store's call, under the incident's lock. Held for the
        // whole resolve+page+record sequence.
        let _guard = self.page_lock(incident_id).lock().await;
        // Sent only while the close still owes it: not twice, not after a
        // reopen withdrew it, and not while the sweep holds it.
        if reason.closes_incident() {
            return match self
                .ops
                .claim_closing_notice(org, incident_id, self.claim_lease_secs())
                .await?
            {
                Some(claim) => self.close_out(org, incident_id, claim).await,
                None => Ok(()),
            };
        }
        let Some(incident) = self.ops.get(org, incident_id).await? else {
            return Ok(());
        };
        // Once the monitor is gone nothing pages.
        if incident.monitor_deleted() {
            return Ok(());
        }
        let Some(target_id) = incident.target_id else {
            return Ok(());
        };
        let Some(target) = self.targets.get(org, target_id).await? else {
            return Ok(());
        };
        match reason {
            // A held monitor rides the pause branch: the writer already skips
            // it, so this only catches an episode that was open when the hold
            // landed. Its closing notice still goes out, above, or an outage
            // that ended would stay on the customer's timeline forever.
            _ if !target.enabled || target.plan_hold_at.is_some() => Ok(()),
            NotificationReason::Opened | NotificationReason::Reopened => {
                self.open_episode(org, &incident, &target, damper).await
            }
            // Claimed above, before the incident is read.
            NotificationReason::Resolved
            | NotificationReason::MonitorDeleted
            | NotificationReason::GroupDeleted => Ok(()),
            // Escalation and reminder pages originate from the sweep, never an
            // inbound signal; a member-down notice from a group's attach.
            NotificationReason::Escalated
            | NotificationReason::Reminder
            | NotificationReason::MemberDown => Ok(()),
            // Silence has no incident row; the silence sweep delivers it.
            NotificationReason::NoData | NotificationReason::DataResumed => Ok(()),
        }
    }

    /// Page the first rung and arm escalation, recorded as one fan-out before
    /// anything is sent. A duplicate Opened signal while the episode is already
    /// paged is a no-op; a monitor with no policy falls back to its bound
    /// channels (the pre-policy behaviour) with no laddered re-paging.
    async fn open_episode(
        &self,
        org: OrgId,
        incident: &OpsIncident,
        target: &Target,
        damper: Damper,
    ) -> Result<()> {
        // Read before anything goes out, so every row this page writes stays
        // this episode's even if the incident reopens while it is sending.
        let Some(episode) = self.ops.generation(org, incident.id).await? else {
            return Ok(());
        };
        // Named by the episode read here, not by whoever asked for the page:
        // a reopen can land between the ask and this read.
        let reason = opening_reason(episode);
        // Retire the last episode's pages before the new one starts, and only
        // those: a second signal for this episode must not stop its own.
        if reason == NotificationReason::Reopened {
            self.cancel_emergency(org, incident.id, Some(episode)).await;
        }
        let releasing = damper == Damper::Skip;
        // The first page an episode owes records once, which the store decides;
        // a release owes nothing and checks here.
        let kind = |arm| {
            if releasing {
                FanOutKind::Release(arm)
            } else {
                FanOutKind::Opening(arm)
            }
        };
        if releasing
            && open_episode_active(
                &self.ops.notifications_for(org, incident.id).await?,
                episode,
            )
        {
            // It still ends its hold, or its scan would find it again every
            // tick.
            self.record_marker(
                Planned::marker(org, incident.id, episode, RELEASED_TRANSPORT, reason),
                None,
                kind(None),
            )
            .await?;
            return Ok(());
        }
        // The scan read `triggered` a sweep ago and a flapping monitor
        // recovers inside that gap. Its all-clear already ran and reached
        // nobody, so paging now announces an outage no recovery would follow.
        // Here because `page` holds the per-incident lock.
        if releasing && incident.state != IncidentState::Triggered {
            return Ok(());
        }
        let mut note = None;
        let mut events = Vec::new();
        // A hand-declared incident is the operator's own signal: the count
        // already excludes them, and holding one would silence a real outage
        // someone declared deliberately. A maintenance window is the same
        // bargain, so it exempts them too.
        let operator_declared = incident.origin == crate::domain::IncidentOrigin::Manual;
        if !operator_declared && self.alerts_suppressed(org, target.id).await {
            // The marker is what the release scan keys off, so a hold is never
            // a drop.
            let held = self
                .record_marker(
                    Planned::marker(org, incident.id, episode, MAINTENANCE_TRANSPORT, reason),
                    Some(
                        "alert held: the monitor is in a maintenance window. It pages \
                         when the window ends if this incident is still open"
                            .to_string(),
                    ),
                    kind(None),
                )
                .await?;
            if held {
                metrics::counter!(crate::metric_names::ALERTS_HELD_MAINTENANCE).increment(1);
            }
            return Ok(());
        }
        if damper == Damper::Apply && !operator_declared {
            match self.flap_state(org, target).await? {
                FlapState::Steady => {}
                FlapState::Crossing => {
                    // Without this the operator just sees alerts stop.
                    events.push((IncidentEventKind::Notified, self.flap_engaged_note(target)));
                    note = Some(self.flap_notice());
                }
                FlapState::Damped => {
                    // Recorded, not delivered: the release scan pages it if the
                    // incident outlasts the hold.
                    let held = self
                        .record_marker(
                            Planned::marker(org, incident.id, episode, DAMPED_TRANSPORT, reason),
                            Some(format!(
                                "alert held: the monitor is flapping. It pages anyway if this \
                                 incident is still open in {} minutes",
                                self.cfg.flap_hold_secs.div_ceil(60)
                            )),
                            kind(None),
                        )
                        .await?;
                    if held {
                        metrics::counter!(crate::metric_names::ALERTS_DAMPED).increment(1);
                    }
                    return Ok(());
                }
            }
        }
        let notice = self.notice(incident, Some(target.name.clone()), reason, note);
        let (mut planned, arm) = match self.policies.resolve_for_target(org, target.id).await? {
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
                        let planned = self
                            .plan_channels(org, incident.id, reason, level, &targets, episode)
                            .await?;
                        if planned.is_empty() {
                            events.push(empty_rung_event(level));
                        }
                        let wait =
                            wait_after(&policy.steps, policy.repeat_count, level, 0, delay_secs);
                        let next_at = Some(Utc::now() + chrono::Duration::seconds(wait.into()));
                        (
                            planned,
                            Some(Arming {
                                policy_id,
                                level,
                                next_at,
                            }),
                        )
                    }
                    // Policy with no steps: record the binding so the console
                    // shows it, but page no one.
                    EscalationDecision::Exhausted => (
                        Planned::new(org, incident.id, episode),
                        Some(Arming {
                            policy_id,
                            level: 0,
                            next_at: None,
                        }),
                    ),
                }
            }
            None => {
                // Propagated on purpose: nothing is written, so the whole
                // episode is tried again. Falling back to the bound channels
                // would record rows, and a recorded episode is one that is
                // never revisited — the rule's channels would then miss the
                // outage entirely rather than be paged a tick late.
                let targets = channel_targets(
                    crate::storage::notification_channels::paging_channel_ids(
                        self.channels.as_ref(),
                        org,
                        target,
                    )
                    .await?,
                );
                let mut planned = self
                    .plan_channels(org, incident.id, reason, 0, &targets, episode)
                    .await?;
                if planned.is_empty() {
                    // Nothing bound, or every bound channel gone: without a row
                    // this episode would be re-run every tick, re-appending its
                    // timeline lines each time.
                    planned.push_marker(UNREACHABLE_TRANSPORT, reason);
                }
                (planned, None)
            }
        };
        if releasing && planned.is_empty() {
            planned.push_marker(RELEASED_TRANSPORT, reason);
        }
        let Some(delivered) = self
            .send_fan_out(planned, events, kind(arm), &notice)
            .await?
        else {
            return Ok(());
        };
        self.log_paged(org, incident.id, reason, delivered).await
    }

    /// Record the one bookkeeping row that stands in for a page, with its
    /// timeline line. False when the store refused it.
    async fn record_marker(
        &self,
        marker: Planned,
        note: Option<String>,
        kind: FanOutKind,
    ) -> Result<bool> {
        let events = note
            .map(|n| (IncidentEventKind::Note, n))
            .into_iter()
            .collect();
        let (fan_out, _) = marker.into_fan_out(events, kind);
        Ok(self.ops.record_fan_out(fan_out).await?.is_some())
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

    fn flap_engaged_note(&self, target: &Target) -> String {
        format!(
            "\"{}\" has opened {} incidents within {} minutes — alerts for further \
             opens are held until it settles",
            target.name,
            self.cfg.flap_max_opens,
            self.cfg.flap_window_secs.div_ceil(60)
        )
    }

    /// Send the closing notice taken under `claim`, then settle it, under the
    /// incident's page lock. Settled whatever came of it (sent, opted out,
    /// nobody to tell) so it is never taken again. An error leaves the lease
    /// to run out and the sweep tries again, skipping whoever was told. A
    /// claim lost mid-way settles nothing: what is left is the next holder's.
    async fn close_out(
        &self,
        org: OrgId,
        incident_id: Uuid,
        mut claim: chrono::DateTime<Utc>,
    ) -> Result<()> {
        // A reopen since the claim withdrew the notice.
        if let Some(incident) = self
            .ops
            .get(org, incident_id)
            .await?
            .filter(|i| i.state == IncidentState::Resolved)
        {
            self.notify_ended(org, &incident, &mut claim).await?;
        }
        self.ops
            .settle_closing_notice(org, incident_id, claim)
            .await
    }

    /// Tell the channels paged this episode that the incident ended, in the
    /// words its close calls for. Once the monitor is gone they still hear it,
    /// under the name the incident kept, and the monitor's recovery opt-out
    /// went with it.
    async fn notify_ended(
        &self,
        org: OrgId,
        incident: &OpsIncident,
        claim: &mut chrono::DateTime<Utc>,
    ) -> Result<()> {
        let reason = incident.closing_reason();
        if incident.monitor_deleted() {
            let name = incident.target_name.clone();
            return self
                .notify_closed(org, incident, name, reason, true, claim)
                .await;
        }
        let Some(target_id) = incident.target_id else {
            return Ok(());
        };
        let Some(target) = self.targets.get(org, target_id).await? else {
            return Ok(());
        };
        let name = Some(target.name.clone());
        self.notify_closed(org, incident, name, reason, target.notify_recovery, claim)
            .await
    }

    /// Tell every channel paged this episode, and not yet told, that the
    /// incident ended: `Resolved` for a recovery or a person's resolve,
    /// `MonitorDeleted` when its monitor's deletion closed it. `send` false
    /// (a recovery opt-out) still stops a repeating emergency page: an ended
    /// incident must go quiet either way. Each channel is told under `claim`,
    /// renewed as it goes.
    async fn notify_closed(
        &self,
        org: OrgId,
        incident: &OpsIncident,
        monitor_name: Option<String>,
        reason: NotificationReason,
        send: bool,
        claim: &mut chrono::DateTime<Utc>,
    ) -> Result<()> {
        self.cancel_emergency(org, incident.id, None).await;
        if !send {
            return Ok(());
        }
        let rows = self.ops.notifications_for(org, incident.id).await?;
        let channels: Vec<Uuid> = resolvable_channels(&rows);
        if channels.is_empty() {
            return Ok(());
        }
        let Some(episode) = self.ops.generation(org, incident.id).await? else {
            return Ok(());
        };
        let notice = self.notice(incident, monitor_name, reason, None);
        let planned = self
            .plan_channels(
                org,
                incident.id,
                reason,
                incident.escalation_level,
                &channel_targets(channels),
                episode,
            )
            .await?;
        // One row at a time, each recorded only while the claim holds and
        // renewing it, so the fan-out stops once the claim is lost. A send can
        // outlast the lease, and another instance taking the notice over then
        // tells only the channels that have no row yet.
        let mut delivered = 0;
        for (row, channel) in planned.into_items() {
            let Some(channel) = channel else {
                continue;
            };
            let Some((id, renewed)) = self
                .ops
                .record_closing_notification(row, *claim, self.claim_lease_secs())
                .await?
            else {
                break;
            };
            *claim = renewed;
            if self.first_attempt(org, &channel, &notice, id).await? == NotificationStatus::Sent {
                delivered += 1;
            }
        }
        self.log_paged(org, incident.id, reason, delivered).await
    }

    /// A lookup failure reads as no maintenance, so an error never silences a page.
    pub(super) async fn alerts_suppressed(&self, org: OrgId, target_id: Uuid) -> bool {
        matches!(
            self.maintenance.alerts_suppressed(org, target_id).await,
            Ok(true)
        )
    }
}
