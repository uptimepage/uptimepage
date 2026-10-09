use std::sync::Arc;
use std::time::Instant;

use chrono::Utc;
use uuid::Uuid;

use crate::domain::{
    EscalationPolicy, EscalationTargetType, NotificationReason, NotificationStatus, OpsIncident,
    OrgId, UserId,
};
use crate::error::Result;
use crate::notifier::event::IncidentNotice;
use crate::notifier::{EmailAlert, build_notifier, notify_following_moves};

use super::Worker;
use super::rules::{PageTarget, Standing, log_error_snippet, push_target, retry_after_hint};
use crate::metric_names;
use crate::security::redaction::redact_url_paths;

#[derive(Clone, Copy, PartialEq)]
enum SendOutcome {
    Sent,
    Failed,
    /// Held back by the transport's own send budget: the retry is the plan, so
    /// it is neither a delivery nor a fault.
    Deferred,
}

impl SendOutcome {
    fn label(self) -> &'static str {
        match self {
            Self::Sent => "sent",
            Self::Failed => "failed",
            Self::Deferred => "deferred",
        }
    }
}

/// Per attempt, not per page: a transport that only succeeds on its third try
/// reads healthy in the row and unhealthy here. Called only where a send was
/// actually made, so a build error never lands a 0 ms sample.
/// `started` is `None` when no send was attempted, i.e. a channel whose stored
/// config will not build. Still a failed notification, but timing it as ~0ms
/// would drag the latency histogram down exactly when a transport is broken.
fn note_send(transport: &str, started: Option<Instant>, outcome: SendOutcome) {
    metrics::counter!(
        metric_names::NOTIFICATIONS_TOTAL,
        "transport" => transport.to_string(),
        "outcome" => outcome.label(),
    )
    .increment(1);
    if outcome == SendOutcome::Deferred {
        return;
    }
    if let Some(started) = started {
        metrics::histogram!(metric_names::NOTIFICATION_DELIVERY_MS, "transport" => transport.to_string())
            .record(started.elapsed().as_millis() as f64);
    }
    if outcome == SendOutcome::Failed {
        metrics::counter!(metric_names::NOTIFICATIONS_FAILURES, "transport" => transport.to_string())
            .increment(1);
    }
}

/// What a retry finds when it re-resolves its page.
fn standing(incident: &crate::domain::OpsIncident) -> Standing {
    Standing {
        state: incident.state,
        recovering: incident.recovering_since.is_some(),
    }
}

pub(super) enum Rebuilt {
    /// The notice to send, its channel, and where the incident stands now for
    /// the staleness check.
    Ready(Box<(IncidentNotice, crate::domain::NotificationChannel, Standing)>),
    /// The incident's monitor was deleted. The incident was closed with it,
    /// so there is nothing left to page about.
    MonitorDeleted,
    /// The channel, monitor or incident is gone. The row exhausts by attempts.
    Gone,
}

impl Worker {
    /// Re-resolve the incident + monitor + channel for a retry.
    pub(super) async fn rebuild_notice(
        &self,
        org: OrgId,
        incident_id: Uuid,
        channel_id: Uuid,
        reason: NotificationReason,
    ) -> Result<Rebuilt> {
        let Some(incident) = self.ops.get(org, incident_id).await? else {
            return Ok(Rebuilt::Gone);
        };
        if incident.monitor_deleted() {
            // Paging went with the monitor. A notice that the incident ended
            // still goes out, under the name the incident kept.
            if !reason.closes_incident() {
                return Ok(Rebuilt::MonitorDeleted);
            }
            let Some(channel) = self.channels.get(org, channel_id).await? else {
                return Ok(Rebuilt::Gone);
            };
            let notice = self.notice(&incident, incident.target_name.clone(), reason, None);
            return Ok(Rebuilt::Ready(Box::new((
                notice,
                channel,
                standing(&incident),
            ))));
        }
        let Some(target_id) = incident.target_id else {
            return Ok(Rebuilt::Gone);
        };
        let Some(target) = self.targets.get(org, target_id).await? else {
            return Ok(Rebuilt::Gone);
        };
        let Some(channel) = self.channels.get(org, channel_id).await? else {
            return Ok(Rebuilt::Gone);
        };
        let notice = self.notice(&incident, Some(target.name.clone()), reason, None);
        Ok(Rebuilt::Ready(Box::new((
            notice,
            channel,
            standing(&incident),
        ))))
    }

    pub(super) async fn deliver(
        &self,
        org: OrgId,
        channel: &crate::domain::NotificationChannel,
        notice: &IncidentNotice,
        notification_id: Uuid,
        attempt: i32,
    ) -> (NotificationStatus, Option<String>, Option<String>) {
        let central = self.central_bot.as_ref().map(|c| c.as_central());
        let email_alert = self.email_alert(org, channel).await;
        let controls = self.alert_controls(org, channel, notice).await;
        let transport = channel.kind.as_db_str();
        let (error, sent_at) = match build_notifier(
            &channel.config,
            &self.http,
            central,
            self.central_whatsapp.as_ref(),
            self.email.as_ref(),
            email_alert,
            controls,
        ) {
            Ok(n) => {
                let started = Instant::now();
                let sent = notify_following_moves(
                    self.channels.as_ref(),
                    org,
                    channel,
                    n.as_ref(),
                    notice,
                )
                .await;
                match sent {
                    Ok(()) => {
                        note_send(transport, Some(started), SendOutcome::Sent);
                        tracing::info!(
                            org_id = %org.0,
                            incident_id = %notice.incident_id,
                            channel_id = %channel.id,
                            notification_id = %notification_id,
                            transport,
                            attempt,
                            took_ms = started.elapsed().as_millis(),
                            "incident notification delivered"
                        );
                        return (NotificationStatus::Sent, None, n.taken_receipt());
                    }
                    Err(err) => (redact_url_paths(&err.to_string()), Some(started)),
                }
            }
            Err(err) => (redact_url_paths(&err.to_string()), None),
        };
        let snippet = log_error_snippet(&error);
        let took_ms = sent_at.map(|s| s.elapsed().as_millis());
        // A throttle hint means deferred, not broken, so the warn stream stays
        // meaningful during a paging burst.
        let deferred =
            channel.kind.provider_throttle_hint() && retry_after_hint(Some(&error)).is_some();
        note_send(
            transport,
            sent_at,
            if deferred {
                SendOutcome::Deferred
            } else {
                SendOutcome::Failed
            },
        );
        if deferred {
            tracing::info!(
                org_id = %org.0,
                incident_id = %notice.incident_id,
                channel_id = %channel.id,
                notification_id = %notification_id,
                transport = channel.kind.as_db_str(),
                attempt,
                took_ms,
                error = %snippet,
                "incident notification deferred by transport"
            );
        } else {
            tracing::warn!(
                org_id = %org.0,
                incident_id = %notice.incident_id,
                channel_id = %channel.id,
                notification_id = %notification_id,
                transport = channel.kind.as_db_str(),
                attempt,
                took_ms,
                error = %snippet,
                "incident notification delivery failed"
            );
        }
        (NotificationStatus::Failed, Some(error), None)
    }

    pub(super) fn notice(
        &self,
        inc: &OpsIncident,
        monitor_name: Option<String>,
        reason: NotificationReason,
        note: Option<String>,
    ) -> IncidentNotice {
        IncidentNotice {
            incident_id: inc.id,
            reason,
            monitor_name,
            title: inc.title.clone(),
            severity: inc.severity,
            urgency: inc.urgency,
            origin: inc.origin,
            started_at: inc.started_at,
            ended_at: inc.ended_at,
            error_sample: inc.error_sample.clone(),
            // The stored split, widened as regions confirm — no per-page region query.
            regions_down: inc.regions_down.clone(),
            regions_up: inc.regions_up.clone(),
            url: self.deep_link(inc.id),
            note,
        }
    }

    fn deep_link(&self, id: Uuid) -> Option<String> {
        let base = self.base_url.trim_end_matches('/');
        (!base.is_empty()).then(|| format!("{base}/incidents/{id}"))
    }

    /// Controls for one page, pinned to the incident's current episode so a
    /// page kept on a phone through a reopen cannot silence or close what
    /// followed.
    pub(super) async fn alert_controls(
        &self,
        org: OrgId,
        channel: &crate::domain::NotificationChannel,
        notice: &IncidentNotice,
    ) -> crate::notifier::AlertControls {
        use crate::domain::AlertAction::{Acknowledge, Resolve};
        let acknowledge = self.control_via(channel, notice, Acknowledge);
        let resolve = self.control_via(channel, notice, Resolve);
        if acknowledge.is_none() && resolve.is_none() {
            return Default::default();
        }
        let generation = match self.ops.generation(org, notice.incident_id).await {
            Ok(Some(g)) => g,
            // Page without the controls rather than mint them for the wrong
            // episode.
            Ok(None) => return Default::default(),
            Err(err) => {
                tracing::warn!(error = %err, "alert control generation lookup failed");
                return Default::default();
            }
        };
        let mint = |via, action| self.mint(org, channel, notice, via, action, generation);
        crate::notifier::AlertControls {
            acknowledge: acknowledge.and_then(|via| mint(via, Acknowledge)),
            resolve: resolve.and_then(|via| mint(via, Resolve)),
        }
    }

    /// How `action` reaches this page: only when the channel offers it, the
    /// incident still awaits it and its transport can deliver it.
    fn control_via(
        &self,
        channel: &crate::domain::NotificationChannel,
        notice: &IncidentNotice,
        action: crate::domain::AlertAction,
    ) -> Option<crate::domain::AlertVia> {
        use crate::domain::{AlertVia, ChannelKind};
        let via = match channel.kind.control_via(action)? {
            // Nothing here would receive the press, so the channel links to the
            // page like a pasted webhook does.
            AlertVia::Button(app) if !self.pressed_apps.contains(&app) => AlertVia::Page,
            via => via,
        };
        if !channel.button(action) || !notice.reason.awaits_acknowledgement() {
            return None;
        }
        let signed = !self.incident_ack_secret.is_empty();
        // Checked before the episode lookup, a query on the paging path.
        let deliverable = match via {
            AlertVia::SignedLink => signed && !self.base_url.is_empty(),
            AlertVia::Button(_) => signed,
            AlertVia::Page => {
                !self.base_url.is_empty()
                    && (channel.kind != ChannelKind::Email || self.email.is_some())
            }
        };
        deliverable.then_some(via)
    }

    fn mint(
        &self,
        org: OrgId,
        channel: &crate::domain::NotificationChannel,
        notice: &IncidentNotice,
        via: crate::domain::AlertVia,
        action: crate::domain::AlertAction,
        generation: i64,
    ) -> Option<crate::notifier::AckControl> {
        use crate::domain::AlertVia;
        use crate::notifier::ack_page::AlertLink;
        use crate::notifier::{AckControl, PushAck};
        match via {
            AlertVia::Button(_) => crate::security::incident_ack::button_data(
                &self.incident_ack_secret,
                action,
                org,
                notice.incident_id,
                channel.id,
                generation,
            )
            .map(AckControl::Button),
            AlertVia::SignedLink => crate::security::incident_ack::link_url(
                &self.base_url,
                &self.incident_ack_secret,
                org,
                notice.incident_id,
                channel.id,
                generation,
                chrono::Utc::now(),
            )
            .map(|url| AckControl::Link(PushAck { url })),
            AlertVia::Page => Some(AckControl::Page(format!(
                "{}{}",
                self.base_url.trim_end_matches('/'),
                AlertLink {
                    org,
                    channel: channel.id,
                    episode: generation,
                }
                .path(action, notice.incident_id)
            ))),
        }
    }

    async fn email_alert(
        &self,
        org: OrgId,
        channel: &crate::domain::NotificationChannel,
    ) -> Option<EmailAlert> {
        crate::notifier::email_alert_for(
            self.orgs.as_ref(),
            &self.base_url,
            &self.alert_channel_stop_secret,
            org,
            channel,
        )
        .await
    }

    /// Resolve a schedule's current on-call roster, served from a short-TTL
    /// cache so a sweep paging many incidents off one schedule loads it once.
    async fn resolve_on_call(
        &self,
        org: OrgId,
        schedule_id: Uuid,
        at: chrono::DateTime<Utc>,
    ) -> Result<Arc<Vec<UserId>>> {
        if let Some(users) = self.on_call_cache.get(&(org, schedule_id)).await {
            return Ok(users);
        }
        let users = Arc::new(
            self.on_call
                .resolve_now(org, schedule_id, at)
                .await?
                .unwrap_or_default(),
        );
        // Don't cache an empty roster: a coverage gap an operator fixes
        // mid-incident must take effect next tick, not after the TTL.
        if !users.is_empty() {
            self.on_call_cache
                .insert((org, schedule_id), users.clone())
                .await;
        }
        Ok(users)
    }

    /// The concrete channels a policy rung pages, resolving each target type:
    /// a `channel` routes straight through; a `user` pages that responder's
    /// contact channels; a `schedule` resolves who is on call at `at` and pages
    /// each of their contact channels. Deduped by channel so one channel is
    /// paged at most once per rung (the first resolving responder is recorded).
    /// A target that resolves to nothing (no contacts, empty schedule) is
    /// skipped and logged.
    pub(super) async fn resolve_targets(
        &self,
        org: OrgId,
        policy: &EscalationPolicy,
        level: i32,
        at: chrono::DateTime<Utc>,
    ) -> Result<Vec<PageTarget>> {
        let Some(step) = policy.steps.iter().find(|s| s.level == level) else {
            return Ok(vec![]);
        };
        let mut out: Vec<PageTarget> = Vec::new();
        for t in &step.targets {
            match t.target_type {
                EscalationTargetType::Channel => {
                    if let Some(cid) = t.channel_id {
                        push_target(&mut out, cid, None);
                    }
                }
                EscalationTargetType::User => {
                    if let Some(uid) = t.user_id {
                        self.page_user(org, UserId(uid), &mut out).await?;
                    }
                }
                EscalationTargetType::Schedule => {
                    if let Some(sid) = t.schedule_id {
                        for user in self.resolve_on_call(org, sid, at).await?.iter().copied() {
                            self.page_user(org, user, &mut out).await?;
                        }
                    }
                }
            }
        }
        if out.is_empty() {
            // A rung that reaches no one is a live misconfiguration (empty
            // schedule, a responder with no contacts, a deleted channel) — the
            // incident escalates past it silently otherwise, so surface it.
            tracing::warn!(policy_id = %policy.id, level, "escalation rung resolved to no reachable channel");
        }
        Ok(out)
    }

    /// Append a responder's contact channels to `out`, attributing each to the
    /// user. A responder with no contact channels reaches no one — logged so a
    /// silently-unreachable on-call is visible in traces.
    async fn page_user(&self, org: OrgId, user: UserId, out: &mut Vec<PageTarget>) -> Result<()> {
        let contacts = self.contacts.for_user(org, user).await?;
        if contacts.is_empty() {
            tracing::warn!(%user, "on-call responder has no contact channels; not paged");
        }
        for cid in contacts {
            push_target(out, cid, Some(user));
        }
        Ok(())
    }
}
