use uuid::Uuid;

use crate::domain::{IncidentOrigin, NotificationReason, OpsIncident, OrgId, Subject};
use crate::error::Result;
use crate::storage::{DueDigest, FanOutKind};
use futures::stream::{FuturesUnordered, StreamExt};
use tokio::time::Instant;

use super::rules::{channel_targets, resolvable_channels};
use super::{SWEEP_CONCURRENCY, Worker};

impl Worker {
    /// What `incident` pages about: its monitor, or for a group incident its
    /// group. `None` once that is gone.
    pub(super) async fn subject(
        &self,
        org: OrgId,
        incident: &OpsIncident,
    ) -> Result<Option<Subject>> {
        if incident.origin == IncidentOrigin::Group {
            let Some(group_id) = incident.group.group_id else {
                return Ok(None);
            };
            return Ok(self.groups.get(org, group_id).await?.map(Subject::Group));
        }
        let Some(target_id) = incident.target_id else {
            return Ok(None);
        };
        Ok(self
            .targets
            .get(org, target_id)
            .await?
            .map(|t| Subject::Monitor(Box::new(t))))
    }

    /// Whether `subject` is watched now: a monitor on and outside a window
    /// that holds alerts, or a group with a watched member down.
    pub(super) async fn watched(
        &self,
        org: OrgId,
        incident: &OpsIncident,
        subject: &Subject,
    ) -> Result<bool> {
        match subject {
            Subject::Monitor(target) => {
                Ok(target.enabled && !self.alerts_suppressed(org, target.id).await)
            }
            Subject::Group(_) => self.ops.group_watched(org, incident.id).await,
        }
    }
}

impl Worker {
    /// Send every member-down digest that has come due.
    pub(super) async fn send_digests(&self) {
        let limit = self.cfg.max_pages_per_tick.max(1) as usize;
        let due = match self.ops.due_member_digests(limit).await {
            Ok(d) => d,
            Err(err) => {
                tracing::warn!(error = %err, "member digest scan failed");
                return;
            }
        };
        let budget = self.sweep_budget();
        let start = Instant::now();
        let mut it = due.into_iter();
        let mut futs = FuturesUnordered::new();
        for d in it.by_ref().take(SWEEP_CONCURRENCY) {
            futs.push(self.digest_one_logged(d));
        }
        while futs.next().await.is_some() {
            if start.elapsed() < budget
                && let Some(d) = it.next()
            {
                futs.push(self.digest_one_logged(d));
            }
        }
    }

    async fn digest_one_logged(&self, d: DueDigest) {
        if let Err(err) = self.digest_one(d.org, d.id).await {
            tracing::warn!(incident_id = %d.id, error = %err, "member digest failed");
        }
    }

    /// Tell the channels this group incident's episode reached which members
    /// went down since its last word about them, when that is due. The store
    /// decides whether it is, under the group's lock. Waits, as reminders do,
    /// while every member down is paused or held.
    pub(super) async fn digest_one(&self, org: OrgId, group_incident: Uuid) -> Result<()> {
        if !self.ops.member_digest_due(org, group_incident).await?
            || !self.ops.group_watched(org, group_incident).await?
        {
            return Ok(());
        }
        let _guard = self.page_lock(group_incident).lock().await;
        let Some(incident) = self.ops.get(org, group_incident).await? else {
            return Ok(());
        };
        if incident.origin != IncidentOrigin::Group || incident.ended_at.is_some() {
            return Ok(());
        }
        let Some(episode) = self.ops.generation(org, group_incident).await? else {
            return Ok(());
        };
        let reached = resolvable_channels(&self.ops.notifications_for(org, group_incident).await?);
        let planned = self
            .plan_channels(
                org,
                group_incident,
                NotificationReason::MemberDown,
                incident.escalation_level,
                &channel_targets(reached),
                episode,
            )
            .await?;
        let notice = self.notice(
            &incident,
            incident.group.group_name.clone(),
            NotificationReason::MemberDown,
            None,
        );
        let Some(delivered) = self
            .send_fan_out(planned, Vec::new(), FanOutKind::MemberDigest, &notice)
            .await?
        else {
            return Ok(());
        };
        self.log_paged(
            org,
            group_incident,
            NotificationReason::MemberDown,
            delivered,
        )
        .await
    }
}
