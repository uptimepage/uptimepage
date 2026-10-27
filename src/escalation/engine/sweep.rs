use chrono::{DateTime, Utc};
use futures::stream::{FuturesUnordered, StreamExt};
use std::sync::Arc;
use tokio::time::Instant;

use uuid::Uuid;

use crate::domain::{
    EscalationDecision, IncidentEventKind, IncidentState, NewIncidentNotification,
    NotificationChannel, NotificationOutcome, NotificationReason, NotificationStatus, OrgId,
    failure_run_reached, next_step, wait_after,
};
use crate::error::Result;
use crate::notifier::event::IncidentNotice;
use crate::storage::{
    Actor, ClaimedEscalation, DueIncident, FanOut, FanOutKind, PendingNotification,
    QUEUED_TAKEOVER_SECS,
};

use super::deliver::{Attempt, Rebuilt};
use super::rules::{PageTarget, channel_targets, marker_row, reason_is_stale, resolvable_channels};
use super::{SWEEP_CONCURRENCY, Worker};

/// Cap on the transport response quoted into the mail: a broken endpoint
/// answers with a page of HTML.
const MAX_MAIL_ERROR_CHARS: usize = 400;

/// Below the retry scan's takeover of a `queued` row, so a first attempt is
/// marked before the scan can send it again.
const FIRST_ATTEMPT_BUDGET: std::time::Duration = std::time::Duration::from_secs(90);
const _: () = assert!(FIRST_ATTEMPT_BUDGET.as_secs() < QUEUED_TAKEOVER_SECS as u64);

/// Everything the owner mail needs, owned, so it can outlive the page that
/// triggered it. Claims inside the task: a lost race costs nothing, while
/// claiming on the caller's thread would put a DB round trip on the page path.
struct FailingChannelMail {
    org: OrgId,
    channel_id: Uuid,
    channel_name: String,
    transport: String,
    consecutive_failures: i32,
    failing_since: Option<DateTime<Utc>>,
    last_error: Option<String>,
    channels: Arc<dyn crate::storage::NotificationChannelStore>,
    orgs: Arc<dyn crate::storage::orgs::OrgDirectory>,
    base_url: String,
    email: crate::notifier::EmailDelivery,
}

impl FailingChannelMail {
    async fn claim_and_send(self) {
        let recipients = match self.orgs.owner_emails(self.org).await {
            Ok(to) => to,
            Err(err) => {
                tracing::warn!(org_id = %self.org.0, error = %err, "owner lookup for channel-failure mail failed");
                return;
            }
        };
        if recipients.is_empty() {
            tracing::warn!(org_id = %self.org.0, channel_id = %self.channel_id, "no owner to tell that a channel stopped delivering");
            return;
        }
        let claimed = match self
            .channels
            .claim_failure_alert(self.org, self.channel_id)
            .await
        {
            Ok(Some(at)) => at,
            Ok(None) => return,
            Err(err) => {
                tracing::warn!(channel_id = %self.channel_id, error = %err, "failure alert not claimed");
                return;
            }
        };
        if self.send_to(&recipients).await {
            return;
        }
        // Unreleased, the run reads as reported and nobody is told until the
        // claim ages out.
        if let Err(err) = self
            .channels
            .release_failure_alert(self.org, self.channel_id, claimed)
            .await
        {
            tracing::warn!(channel_id = %self.channel_id, error = %err, "failure alert not released");
        }
    }

    /// `true` once at least one owner has the mail.
    async fn send_to(&self, recipients: &[String]) -> bool {
        let org_name = self.orgs.display_name(self.org).await.ok().flatten();
        let base = self.base_url.trim_end_matches('/');
        let channel_url = (!base.is_empty())
            .then(|| format!("{base}/settings/notifications/{}/edit", self.channel_id));
        let template = crate::email::EmailTemplate::ChannelFailing {
            channel_name: self.channel_name.clone(),
            transport: self.transport.clone(),
            org_name,
            consecutive_failures: self.consecutive_failures,
            failing_secs: self
                .failing_since
                .map(|since| (Utc::now() - since).num_seconds()),
            last_error: self.last_error.clone(),
            channel_url,
        };
        let mut told = false;
        for to in recipients {
            let outgoing = crate::email::TransactionalEmail {
                from: crate::email::EmailAddress::new(
                    self.email.from_address.clone(),
                    self.email.from_name.clone(),
                ),
                to: crate::email::EmailAddress::new(to.clone(), to.clone()),
                template: template.clone(),
            };
            match self
                .email
                .sender
                .send(outgoing, crate::email::SendClass::Immediate)
                .await
            {
                Ok(_) => told = true,
                Err(err) => {
                    tracing::warn!(org_id = %self.org.0, error = %err, "channel-failure mail not sent");
                }
            }
        }
        told
    }
}

/// How long a quiet monitor's escalation timer parks before the sweep looks
/// at it again.
const SUPPRESSED_ESCALATION_RETRY_SECS: i64 = 60;

/// A page's rows before they are recorded, for one incident's episode, each
/// with the channel it goes to, or none for a bookkeeping row.
pub(super) struct Planned {
    org: OrgId,
    incident_id: Uuid,
    episode: i64,
    items: Vec<(NewIncidentNotification, Option<NotificationChannel>)>,
}

impl Planned {
    pub(super) fn new(org: OrgId, incident_id: Uuid, episode: i64) -> Self {
        Self {
            org,
            incident_id,
            episode,
            items: Vec::new(),
        }
    }

    /// The one bookkeeping row that stands in for a page.
    pub(super) fn marker(
        org: OrgId,
        incident_id: Uuid,
        episode: i64,
        transport: &str,
        reason: NotificationReason,
    ) -> Self {
        let mut planned = Self::new(org, incident_id, episode);
        planned.push_marker(transport, reason);
        planned
    }

    pub(super) fn push_marker(&mut self, transport: &str, reason: NotificationReason) {
        let row = marker_row(self.org, self.incident_id, transport, reason, self.episode);
        self.items.push((row, None));
    }

    pub(super) fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub(super) fn into_items(self) -> Vec<(NewIncidentNotification, Option<NotificationChannel>)> {
        self.items
    }

    /// The fan-out that records these rows, and the channel each row goes to.
    pub(super) fn into_fan_out(
        self,
        events: Vec<(IncidentEventKind, String)>,
        kind: FanOutKind,
    ) -> (FanOut, Vec<Option<NotificationChannel>>) {
        let (rows, channels) = self.items.into_iter().unzip();
        let fan_out = FanOut {
            org: self.org,
            incident_id: self.incident_id,
            episode: self.episode,
            rows,
            events,
            kind,
        };
        (fan_out, channels)
    }
}

impl Worker {
    /// Walk the next rung of every due incident's policy, bounded-concurrent so
    /// one slow channel never serialises the tick, and budget-capped so the
    /// sweep returns promptly (the remainder is picked up next tick).
    pub(super) async fn escalate_due(&self) {
        let limit = self.cfg.max_pages_per_tick.max(1) as usize;
        let lease = self.claim_lease_secs();
        let due = match self.ops.due_for_escalation(Utc::now(), limit, lease).await {
            Ok(d) => d,
            Err(err) => {
                tracing::warn!(error = %err, "escalation sweep scan failed");
                return;
            }
        };
        let budget = self.sweep_budget();
        let start = Instant::now();
        let mut it = due.into_iter();
        let mut futs = FuturesUnordered::new();
        for d in it.by_ref().take(SWEEP_CONCURRENCY) {
            futs.push(self.escalate_one_logged(d));
        }
        while futs.next().await.is_some() {
            if start.elapsed() < budget
                && let Some(d) = it.next()
            {
                futs.push(self.escalate_one_logged(d));
            }
        }
    }

    async fn escalate_one_logged(&self, c: ClaimedEscalation) {
        if let Err(err) = self.escalate_one(&c).await {
            tracing::warn!(incident_id = %c.due.id, error = %err, "escalation step failed");
        }
    }

    /// Re-page the channels of open, unacknowledged incidents whose monitor's
    /// reminder interval has elapsed since the last page. Bounded-concurrent +
    /// budget-capped like the escalation sweep. Unlike `escalate_due` this takes
    /// no claim-and-lease: the single-tick single-flight guard already prevents
    /// an instance from reminding the same incident twice per interval, and a
    /// duplicate across two engine instances costs one extra page plus a
    /// double-widened backoff, never a skipped rung.
    pub(super) async fn renotify_due(&self) {
        let limit = self.cfg.max_pages_per_tick.max(1) as usize;
        let due = match self.ops.due_for_renotify(Utc::now(), limit).await {
            Ok(d) => d,
            Err(err) => {
                tracing::warn!(error = %err, "renotify scan failed");
                return;
            }
        };
        let budget = self.sweep_budget();
        let start = Instant::now();
        let mut it = due.into_iter();
        let mut futs = FuturesUnordered::new();
        for d in it.by_ref().take(SWEEP_CONCURRENCY) {
            futs.push(self.renotify_one_logged(d));
        }
        while futs.next().await.is_some() {
            if start.elapsed() < budget
                && let Some(d) = it.next()
            {
                futs.push(self.renotify_one_logged(d));
            }
        }
    }

    async fn renotify_one_logged(&self, d: DueIncident) {
        if let Err(err) = self.renotify_one(&d).await {
            tracing::warn!(incident_id = %d.id, error = %err, "incident reminder failed");
        }
    }

    /// Re-page the channels already notified this episode for a still-open,
    /// unacknowledged incident. Re-reads state under the per-incident lock so an
    /// ack/resolve landing after the scan silences the reminder; the fresh page
    /// advances the last-paged time and widens the gap before the next one.
    pub(super) async fn renotify_one(&self, d: &DueIncident) -> Result<()> {
        let _guard = self.page_lock(d.id).lock().await;
        let Some(incident) = self.ops.get(d.org, d.id).await? else {
            return Ok(());
        };
        if incident.state != IncidentState::Triggered || incident.recovering_since.is_some() {
            return Ok(());
        }
        let Some(target_id) = incident.target_id else {
            return Ok(());
        };
        let Some(target) = self.targets.get(d.org, target_id).await? else {
            return Ok(());
        };
        if !target.enabled || target.renotify_interval_secs == 0 {
            return Ok(());
        }
        if self.alerts_suppressed(d.org, target_id).await {
            return Ok(());
        }
        let channels = resolvable_channels(&self.ops.notifications_for(d.org, d.id).await?);
        if channels.is_empty() {
            return Ok(());
        }
        let Some(episode) = self.ops.generation(d.org, d.id).await? else {
            return Ok(());
        };
        let notice = self.notice(
            &incident,
            Some(target.name.clone()),
            NotificationReason::Reminder,
            None,
        );
        let planned = self
            .plan_channels(
                d.org,
                d.id,
                NotificationReason::Reminder,
                incident.escalation_level,
                &channel_targets(channels),
                episode,
            )
            .await?;
        if planned.is_empty() {
            return Ok(());
        }
        let Some(delivered) = self
            .send_fan_out(planned, Vec::new(), FanOutKind::Reminder, &notice)
            .await?
        else {
            return Ok(());
        };
        self.log_paged(d.org, d.id, NotificationReason::Reminder, delivered)
            .await
    }

    /// Take the step `c` claimed, unless the incident has moved on since: an
    /// ack or a resolve cleared the timer, a reopen's first page set its own,
    /// or another instance retook the step once the lease ran out. Under the
    /// page lock, so a step and an opening page never interleave. The step
    /// records only while the claim still holds it.
    pub(super) async fn escalate_one(&self, c: &ClaimedEscalation) -> Result<()> {
        let d = &c.due;
        let _guard = self.page_lock(d.id).lock().await;
        let Some(incident) = self.ops.get(d.org, d.id).await? else {
            return Ok(());
        };
        if incident.next_escalation_at != Some(c.claim) {
            return Ok(());
        }
        let Some(policy_id) = d.escalation_policy_id else {
            // Timer armed without a policy — disarm so it does not spin.
            self.ops
                .record_escalation(d.org, d.id, d.escalation_level, d.escalation_round, None)
                .await?;
            return Ok(());
        };
        // A deleted policy stops escalation cleanly.
        let Some(policy) = self.policies.get(d.org, policy_id).await? else {
            self.ops
                .record_escalation(d.org, d.id, d.escalation_level, d.escalation_round, None)
                .await?;
            return Ok(());
        };
        match next_step(
            &policy.steps,
            policy.repeat_count,
            d.escalation_level,
            d.escalation_round,
        ) {
            EscalationDecision::Page {
                level,
                round,
                delay_secs,
            } => {
                let Some(target_id) = incident.target_id else {
                    return Ok(());
                };
                let Some(target) = self.targets.get(d.org, target_id).await? else {
                    return Ok(());
                };
                // Push the timer out rather than clearing it: nothing re-arms a
                // cleared one but a fresh open, so the rest of the ladder would
                // never page once the monitor came back. A recovery held since
                // the claim waits the same way, in case the failure returns.
                if !target.enabled
                    || incident.recovering_since.is_some()
                    || self.alerts_suppressed(d.org, target.id).await
                {
                    let next_at =
                        Utc::now() + chrono::Duration::seconds(SUPPRESSED_ESCALATION_RETRY_SECS);
                    self.ops
                        .record_escalation(
                            d.org,
                            d.id,
                            d.escalation_level,
                            d.escalation_round,
                            Some(next_at),
                        )
                        .await?;
                    return Ok(());
                }
                let notice = self.notice(
                    &incident,
                    Some(target.name.clone()),
                    NotificationReason::Escalated,
                    None,
                );
                let targets = self
                    .resolve_targets(d.org, &policy, level, Utc::now())
                    .await?;
                let planned = self
                    .plan_channels(
                        d.org,
                        d.id,
                        NotificationReason::Escalated,
                        level,
                        &targets,
                        c.episode,
                    )
                    .await?;
                let events = if planned.is_empty() {
                    vec![empty_rung_event(level)]
                } else {
                    Vec::new()
                };
                let wait = wait_after(&policy.steps, policy.repeat_count, level, round, delay_secs);
                let rung = FanOutKind::Rung {
                    claim: c.claim,
                    level,
                    round,
                    next_at: Some(Utc::now() + chrono::Duration::seconds(wait.into())),
                };
                let Some(delivered) = self.send_fan_out(planned, events, rung, &notice).await?
                else {
                    return Ok(());
                };
                self.log_paged(d.org, d.id, NotificationReason::Escalated, delivered)
                    .await?;
            }
            EscalationDecision::Exhausted => {
                self.ops
                    .record_escalation(d.org, d.id, d.escalation_level, d.escalation_round, None)
                    .await?;
                self.ops
                    .append_event(
                        d.org,
                        d.id,
                        IncidentEventKind::Escalated,
                        Actor::System,
                        Some("escalation policy exhausted".into()),
                    )
                    .await?;
            }
        }
        Ok(())
    }

    /// A `queued` row for every enabled channel among `targets`, stamped with
    /// `episode`, the one the page was decided for. The caller pre-filters the
    /// set (dedup, recovery opt-out). A channel that cannot be read fails the
    /// whole page, which then records nothing and is tried again whole.
    pub(super) async fn plan_channels(
        &self,
        org: OrgId,
        incident_id: Uuid,
        reason: NotificationReason,
        level: i32,
        targets: &[PageTarget],
        episode: i64,
    ) -> Result<Planned> {
        let mut planned = Planned::new(org, incident_id, episode);
        for t in targets {
            let Some(channel) = self.channels.get(org, t.channel_id).await? else {
                continue;
            };
            if !channel.enabled {
                continue;
            }
            let row = NewIncidentNotification {
                org,
                incident_id,
                escalation_level: Some(level),
                target_user_id: t.user_id,
                channel_id: Some(channel.id),
                transport: channel.kind.as_db_str().to_string(),
                reason,
                status: NotificationStatus::Queued,
                attempt: 1,
                error: None,
                sent_at: None,
                episode,
            };
            planned.items.push((row, Some(channel)));
        }
        Ok(planned)
    }

    /// Record `planned` whole, with its timeline lines and `kind`'s
    /// bookkeeping, then make the first attempt on every channel row at once.
    /// Concurrent, so every first attempt ends inside its budget of the commit,
    /// before the retry sweep may take a `queued` row over. `None` when the
    /// store refused the page; otherwise how many delivered. A row that cannot
    /// be settled is the retry sweep's once it is past the takeover; the rest
    /// still count.
    pub(super) async fn send_fan_out(
        &self,
        planned: Planned,
        events: Vec<(IncidentEventKind, String)>,
        kind: FanOutKind,
        notice: &IncidentNotice,
    ) -> Result<Option<u32>> {
        let org = planned.org;
        let (fan_out, channels) = planned.into_fan_out(events, kind);
        let Some(ids) = self.ops.record_fan_out(fan_out).await? else {
            return Ok(None);
        };
        let attempts = ids
            .into_iter()
            .zip(channels)
            .filter_map(|(id, channel)| channel.map(|channel| (id, channel)))
            .map(|(id, channel)| async move {
                (id, self.first_attempt(org, &channel, notice, id).await)
            });
        let mut delivered = 0;
        for (id, status) in futures::future::join_all(attempts).await {
            match status {
                Ok(NotificationStatus::Sent) => delivered += 1,
                Ok(_) => {}
                Err(err) => {
                    tracing::warn!(notification_id = %id, error = %err, "first attempt not settled");
                }
            }
        }
        Ok(Some(delivered))
    }

    /// Send recorded row `id` for the first time, within
    /// [`FIRST_ATTEMPT_BUDGET`], and settle it.
    pub(super) async fn first_attempt(
        &self,
        org: OrgId,
        channel: &NotificationChannel,
        notice: &IncidentNotice,
        id: Uuid,
    ) -> Result<NotificationStatus> {
        let attempt = match tokio::time::timeout(
            FIRST_ATTEMPT_BUDGET,
            self.deliver(org, channel, notice, id, 1),
        )
        .await
        {
            Ok(outcome) => outcome,
            Err(_) => {
                tracing::warn!(
                    org_id = %org.0,
                    incident_id = %notice.incident_id,
                    channel_id = %channel.id,
                    notification_id = %id,
                    transport = channel.kind.as_db_str(),
                    "incident notification first attempt exceeded its budget"
                );
                (
                    NotificationStatus::Failed,
                    Some(format!("no outcome within {FIRST_ATTEMPT_BUDGET:?}")),
                    None,
                )
            }
        };
        let status = attempt.0;
        self.settle_attempt(org, channel, id, 1, attempt).await?;
        Ok(status)
    }

    /// Store what attempt `attempt` on row `id` came to, then fold it into the
    /// channel's failure run. The row goes first: until it lands the retry
    /// sweep may take the row over and send it again, while a lost health
    /// update costs only one outcome in the run. A failure schedules the
    /// backoff so the retry sweep waits instead of re-firing next tick.
    async fn settle_attempt(
        &self,
        org: OrgId,
        channel: &NotificationChannel,
        id: Uuid,
        attempt: i32,
        (status, error, receipt): Attempt,
    ) -> Result<()> {
        let next_attempt_at = (status == NotificationStatus::Failed)
            .then(|| self.retry_backoff_hinted(attempt, error.as_deref()))
            .flatten();
        if status == NotificationStatus::Failed {
            self.note_dead_letter(channel.kind.as_db_str(), next_attempt_at);
        }
        self.ops
            .mark_notification(
                org,
                id,
                NotificationOutcome {
                    status,
                    attempt,
                    error: error.clone(),
                    sent_at: (status == NotificationStatus::Sent).then(Utc::now),
                    next_attempt_at,
                    provider_receipt: receipt,
                },
            )
            .await?;
        self.record_channel_health(org, channel, status, next_attempt_at, error.as_deref())
            .await;
        Ok(())
    }

    /// Fold a finished delivery into the channel's failure run. Only one that
    /// has run out of retries counts: one still due a retry has not failed yet.
    async fn record_channel_health(
        &self,
        org: OrgId,
        channel: &NotificationChannel,
        status: NotificationStatus,
        next_attempt_at: Option<DateTime<Utc>>,
        error: Option<&str>,
    ) {
        // An unverified address fails every delivery by design, and the
        // console already says so.
        if channel.awaiting_verification() {
            return;
        }
        let delivered = match status {
            NotificationStatus::Sent => true,
            NotificationStatus::Failed if next_attempt_at.is_none() => false,
            _ => return,
        };
        let health = match self
            .channels
            .record_delivery_outcome(org, channel.id, delivered)
            .await
        {
            Ok(health) => health,
            Err(err) => {
                tracing::warn!(channel_id = %channel.id, error = %err, "channel delivery run not recorded");
                return;
            }
        };
        if delivered
            || !failure_run_reached(health.consecutive_failures, self.cfg.channel_failure_limit)
        {
            return;
        }
        tracing::warn!(
            org_id = %org.0,
            channel_id = %channel.id,
            transport = channel.kind.as_db_str(),
            consecutive_failures = health.consecutive_failures,
            "notification channel is not delivering"
        );
        // Claiming with no way to send would burn the run's one report.
        let Some(email) = self.email.clone() else {
            return;
        };
        // Detached: this runs inside the incident's page lock, and a slow mail
        // provider must not hold up the channels queued behind this one.
        tokio::spawn(
            FailingChannelMail {
                org,
                channel_id: channel.id,
                channel_name: channel.name.clone(),
                transport: channel.kind.as_db_str().to_string(),
                consecutive_failures: health.consecutive_failures,
                failing_since: health.failing_since,
                last_error: error.map(|e| crate::text::truncate_chars(e, MAX_MAIL_ERROR_CHARS)),
                channels: self.channels.clone(),
                orgs: self.orgs.clone(),
                base_url: self.base_url.clone(),
                email,
            }
            .claim_and_send(),
        );
    }

    pub(super) async fn log_paged(
        &self,
        org: OrgId,
        incident_id: Uuid,
        reason: NotificationReason,
        paged: u32,
    ) -> Result<()> {
        if paged == 0 {
            return Ok(());
        }
        let kind = match reason {
            NotificationReason::Escalated => IncidentEventKind::Escalated,
            _ => IncidentEventKind::Notified,
        };
        self.ops
            .append_event(
                org,
                incident_id,
                kind,
                Actor::System,
                Some(format!("paged {paged} channel(s): {}", reason.as_db_str())),
            )
            .await
    }

    /// Re-attempt failed deliveries under the attempt cap. Each retry updates
    /// the existing row rather than inserting a new one. Bounded-concurrent +
    /// budget-capped like the escalation sweep.
    pub(super) async fn retry_pending(&self) {
        let max_attempts = self.cfg.max_attempts as i32;
        let limit = self.cfg.max_pages_per_tick.max(1) as usize;
        let pending = match self
            .ops
            .pending_notifications(Utc::now(), limit, max_attempts)
            .await
        {
            Ok(p) => p,
            Err(err) => {
                tracing::warn!(error = %err, "escalation retry scan failed");
                return;
            }
        };
        let budget = self.sweep_budget();
        let start = Instant::now();
        let mut it = pending.into_iter();
        let mut futs = FuturesUnordered::new();
        for p in it.by_ref().take(SWEEP_CONCURRENCY) {
            futs.push(self.retry_one_logged(p));
        }
        while futs.next().await.is_some() {
            if start.elapsed() < budget
                && let Some(p) = it.next()
            {
                futs.push(self.retry_one_logged(p));
            }
        }
    }

    async fn retry_one_logged(&self, p: PendingNotification) {
        if let Err(err) = self.retry_one(&p).await {
            tracing::warn!(notification_id = %p.id, error = %err, "incident page retry failed");
        }
    }

    async fn retry_one(&self, p: &PendingNotification) -> Result<()> {
        let next_attempt = p.attempt + 1;
        // Resolve the same channel the original page targeted. A missing
        // channel/monitor/incident, a channel turned off since the page was
        // queued, or a page whose reason no longer matches the incident's state
        // (e.g. an Opened page after the incident already resolved) is dropped
        // so on-call never gets a stale notice. Burning an attempt lets the row
        // exhaust instead of retrying forever.
        let rebuilt = match p.channel_id {
            Some(cid) => {
                self.rebuild_notice(p.org, p.incident_id, cid, p.reason)
                    .await?
            }
            None => Rebuilt::Gone,
        };
        if matches!(rebuilt, Rebuilt::MonitorDeleted) {
            // Not a delivery failure: a page caught mid-flight by the delete
            // has nothing left to say.
            self.ops
                .mark_notification(
                    p.org,
                    p.id,
                    NotificationOutcome {
                        status: NotificationStatus::Suppressed,
                        attempt: next_attempt,
                        error: Some("monitor deleted".to_string()),
                        sent_at: None,
                        next_attempt_at: None,
                        provider_receipt: None,
                    },
                )
                .await?;
            return Ok(());
        }
        let Rebuilt::Ready(ready) = rebuilt else {
            // Channel/incident gone: back off so a dead target doesn't churn
            // every tick, and let the attempt cap retire the row.
            let next_attempt_at = self.retry_backoff(next_attempt);
            self.note_dead_letter(&p.transport, next_attempt_at);
            self.ops
                .mark_notification(
                    p.org,
                    p.id,
                    NotificationOutcome {
                        status: NotificationStatus::Failed,
                        attempt: next_attempt,
                        error: Some("channel, monitor or incident no longer exists".to_string()),
                        sent_at: None,
                        next_attempt_at,
                        provider_receipt: None,
                    },
                )
                .await?;
            return Ok(());
        };
        let (notice, channel, now) = *ready;
        // Terminal either way, but the two causes read alike on the row unless
        // it says which.
        let suppressed = if !channel.enabled {
            Some("channel was turned off before the retry landed")
        } else if reason_is_stale(p.reason, now) {
            Some("incident state moved on before the retry landed")
        } else {
            None
        };
        if let Some(reason) = suppressed {
            tracing::info!(
                org_id = %p.org.0,
                incident_id = %p.incident_id,
                channel_id = %channel.id,
                notification_id = %p.id,
                transport = %p.transport,
                attempt = next_attempt,
                reason,
                "incident notification suppressed"
            );
            self.ops
                .mark_notification(
                    p.org,
                    p.id,
                    NotificationOutcome {
                        status: NotificationStatus::Suppressed,
                        attempt: next_attempt,
                        error: Some(reason.to_string()),
                        sent_at: None,
                        next_attempt_at: None,
                        provider_receipt: None,
                    },
                )
                .await?;
            return Ok(());
        }
        let attempt = self
            .deliver(p.org, &channel, &notice, p.id, next_attempt)
            .await;
        self.settle_attempt(p.org, &channel, p.id, next_attempt, attempt)
            .await
    }
}

/// The timeline line for a policy rung that paged no one (empty schedule, a
/// responder with no contacts, every channel deleted or turned off), so the
/// incident does not silently escalate past an unreachable rung.
pub(super) fn empty_rung_event(level: i32) -> (IncidentEventKind, String) {
    (
        IncidentEventKind::Escalated,
        format!("escalation level {level} reached no channel — check the policy's targets"),
    )
}
