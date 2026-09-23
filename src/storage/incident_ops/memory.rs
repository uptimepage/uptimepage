//! Single-tenant in-memory [`IncidentOpsStore`] double for tests: matches on id
//! alone and keeps the same state machine as the Postgres store.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use serde_json::Value;
use uuid::Uuid;

use crate::domain::{
    IncidentEvent, IncidentEventKind, IncidentMetrics, IncidentNotification, IncidentOrigin,
    IncidentSeverity, IncidentState, IncidentTransition, IncidentVisibility, MetricBucket,
    MonitorIncidentCount, NewIncidentNotification, NewManualIncident, NotificationOutcome,
    NotificationStatus, OpsIncident, OrgId, UserId, next_state,
};
use crate::error::Result;

use super::{
    Actor, DueIncident, EmergencyAck, INCIDENT_DETAIL_ROW_CAP, IncidentOpsFilter, IncidentOpsStore,
    IncidentSort, IncidentStateCounts, LifecycleOutcome, PendingNotification, QUEUED_TAKEOVER_SECS,
    pages_with_monitor, status_page_required,
};

#[derive(Default)]
pub struct InMemoryIncidentOpsStore {
    inner: Mutex<MemState>,
}

#[derive(Default)]
struct MemState {
    incidents: Vec<OpsIncident>,
    events: Vec<IncidentEvent>,
    notifications: Vec<(OrgId, IncidentNotification)>,
    renotify_counts: std::collections::HashMap<Uuid, u32>,
    status_pages: std::collections::HashMap<Uuid, Vec<Uuid>>,
}

/// Episode number off the timeline, matching the Postgres store.
fn reopen_count(state: &MemState, incident_id: Uuid) -> i64 {
    reopens_before(state, incident_id, Utc::now())
}

/// The same count as it stood at `at`.
fn reopens_before(state: &MemState, incident_id: Uuid, at: DateTime<Utc>) -> i64 {
    state
        .events
        .iter()
        .filter(|e| {
            e.incident_id == incident_id
                && e.kind == IncidentEventKind::Reopened
                && e.occurred_at <= at
        })
        .count() as i64
}

impl InMemoryIncidentOpsStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn seed(&self, incident: OpsIncident) {
        self.inner.lock().incidents.push(incident);
    }

    /// Test helper: edit a seeded incident in place, for fields no lifecycle
    /// call sets.
    #[cfg(test)]
    pub fn edit(&self, id: Uuid, f: impl FnOnce(&mut OpsIncident)) {
        if let Some(i) = self.inner.lock().incidents.iter_mut().find(|i| i.id == id) {
            f(i);
        }
    }

    /// Test helper: reminders counted against an incident. The backoff itself
    /// is SQL, so only the counting is observable here.
    pub fn renotify_count(&self, id: Uuid) -> u32 {
        self.inner
            .lock()
            .renotify_counts
            .get(&id)
            .copied()
            .unwrap_or(0)
    }

    /// Test helper: backdate every held row, simulating the flap hold elapsing.
    #[cfg(test)]
    pub fn age_held_rows(&self, by: chrono::Duration) {
        for (_, n) in self.inner.lock().notifications.iter_mut() {
            if n.channel_id.is_none() {
                n.created_at -= by;
            }
        }
    }

    /// Test helper: clear every notification's retry backoff so the next
    /// `retry_pending` treats them as due (simulates the backoff elapsing).
    #[cfg(test)]
    pub fn clear_retry_backoff(&self) {
        for (_, n) in self.inner.lock().notifications.iter_mut() {
            n.next_attempt_at = None;
        }
    }

    fn push_event(
        state: &mut MemState,
        incident_id: Uuid,
        kind: IncidentEventKind,
        actor: Actor,
        message: Option<String>,
    ) {
        state.events.push(IncidentEvent {
            id: Uuid::now_v7(),
            incident_id,
            occurred_at: Utc::now(),
            kind,
            actor_type: actor.actor_type(),
            actor_id: actor.user_id(),
            detail: Value::Object(Default::default()),
            message,
        });
    }

    fn generation_of(&self, id: Uuid) -> Option<i64> {
        let g = self.inner.lock();
        g.incidents
            .iter()
            .any(|i| i.id == id)
            .then(|| reopen_count(&g, id))
    }

    #[allow(clippy::too_many_arguments)]
    fn apply(
        &self,
        id: Uuid,
        transition: IncidentTransition,
        kind: IncidentEventKind,
        actor: Actor,
        note: Option<String>,
        expect_generation: Option<i64>,
        mutate: impl FnOnce(&mut OpsIncident),
    ) -> LifecycleOutcome {
        let mut g = self.inner.lock();
        let Some(idx) = g.incidents.iter().position(|i| i.id == id) else {
            return LifecycleOutcome::NotFound;
        };
        if let Some(expected) = expect_generation
            && reopen_count(&g, id) != expected
        {
            return LifecycleOutcome::Stale;
        }
        let from = g.incidents[idx].state;
        let to = match next_state(from, transition) {
            Ok(to) => to,
            Err(err) => return LifecycleOutcome::IllegalTransition(err),
        };
        // Mirrors the Postgres store: a transition that moves nothing leaves
        // the row alone but still records that somebody acted.
        if from != to {
            mutate(&mut g.incidents[idx]);
            g.incidents[idx].updated_at = Utc::now();
        }
        let updated = g.incidents[idx].clone();
        Self::push_event(&mut g, id, kind, actor, note);
        LifecycleOutcome::Updated(Box::new(updated))
    }
}

fn sev_rank(s: IncidentSeverity) -> u8 {
    match s {
        IncidentSeverity::Critical => 3,
        IncidentSeverity::Major => 2,
        IncidentSeverity::Minor => 1,
    }
}

fn in_mem_matches_query(inc: &OpsIncident, needle: Option<&str>) -> bool {
    needle.is_none_or(|n| {
        inc.title
            .as_deref()
            .is_some_and(|t| t.to_lowercase().contains(n))
    })
}

#[async_trait]
impl IncidentOpsStore for InMemoryIncidentOpsStore {
    async fn get(&self, _org: OrgId, id: Uuid) -> Result<Option<OpsIncident>> {
        Ok(self
            .inner
            .lock()
            .incidents
            .iter()
            .find(|i| i.id == id)
            .cloned())
    }

    async fn list(&self, _org: OrgId, filter: IncidentOpsFilter) -> Result<Vec<OpsIncident>> {
        let needle = filter
            .query
            .as_deref()
            .filter(|s| !s.is_empty())
            .map(str::to_lowercase);
        let mut out: Vec<OpsIncident> = self
            .inner
            .lock()
            .incidents
            .iter()
            .filter(|i| filter.state.is_none_or(|s| i.state == s))
            .filter(|i| filter.severity.is_none_or(|s| i.severity == s))
            .filter(|i| filter.assignee.is_none_or(|u| i.assigned_to == Some(u)))
            .filter(|i| in_mem_matches_query(i, needle.as_deref()))
            .cloned()
            .collect();
        match filter.sort {
            IncidentSort::Recent => out.sort_by_key(|i| std::cmp::Reverse(i.started_at)),
            IncidentSort::Oldest => out.sort_by_key(|i| i.started_at),
            IncidentSort::Severity => out.sort_by(|a, b| {
                sev_rank(b.severity)
                    .cmp(&sev_rank(a.severity))
                    .then(b.started_at.cmp(&a.started_at))
            }),
        }
        let lim = filter.limit.unwrap_or(100);
        Ok(out.into_iter().skip(filter.offset).take(lim).collect())
    }

    async fn count(&self, _org: OrgId, filter: &IncidentOpsFilter) -> Result<usize> {
        let needle = filter
            .query
            .as_deref()
            .filter(|s| !s.is_empty())
            .map(str::to_lowercase);
        Ok(self
            .inner
            .lock()
            .incidents
            .iter()
            .filter(|i| filter.state.is_none_or(|s| i.state == s))
            .filter(|i| filter.severity.is_none_or(|s| i.severity == s))
            .filter(|i| filter.assignee.is_none_or(|u| i.assigned_to == Some(u)))
            .filter(|i| in_mem_matches_query(i, needle.as_deref()))
            .count())
    }

    async fn counts_by_state(
        &self,
        _org: OrgId,
        filter: &IncidentOpsFilter,
    ) -> Result<IncidentStateCounts> {
        let needle = filter
            .query
            .as_deref()
            .filter(|s| !s.is_empty())
            .map(str::to_lowercase);
        let g = self.inner.lock();
        let mut c = IncidentStateCounts::default();
        for i in g.incidents.iter().filter(|i| {
            filter.severity.is_none_or(|s| i.severity == s)
                && filter.assignee.is_none_or(|u| i.assigned_to == Some(u))
                && in_mem_matches_query(i, needle.as_deref())
        }) {
            match i.state {
                IncidentState::Triggered => c.triggered += 1,
                IncidentState::Acknowledged => c.acknowledged += 1,
                IncidentState::Resolved => c.resolved += 1,
            }
        }
        Ok(c)
    }

    async fn metrics(&self, _org: OrgId, window_days: u32) -> Result<IncidentMetrics> {
        let since = Utc::now() - chrono::Duration::days(window_days.clamp(1, 365) as i64);
        let g = self.inner.lock();
        let in_window: Vec<&OpsIncident> = g
            .incidents
            .iter()
            .filter(|i| i.started_at >= since)
            .collect();

        let mean = |vals: &[f64]| -> Option<f64> {
            (!vals.is_empty()).then(|| vals.iter().sum::<f64>() / vals.len() as f64)
        };
        let mtta: Vec<f64> = in_window
            .iter()
            .filter_map(|i| {
                i.acknowledged_at
                    .map(|a| (a - i.started_at).num_seconds() as f64)
            })
            .collect();
        let mttr: Vec<f64> = in_window
            .iter()
            .filter_map(|i| i.ended_at.map(|e| (e - i.started_at).num_seconds() as f64))
            .collect();

        let tally = |key_of: &dyn Fn(&OpsIncident) -> String| {
            let mut counts: std::collections::HashMap<String, u64> = Default::default();
            for i in &in_window {
                *counts.entry(key_of(i)).or_default() += 1;
            }
            let mut v: Vec<MetricBucket> = counts
                .into_iter()
                .map(|(key, count)| MetricBucket { key, count })
                .collect();
            v.sort_by(|a, b| b.count.cmp(&a.count).then(a.key.cmp(&b.key)));
            v
        };

        let mut top: std::collections::HashMap<Uuid, u64> = Default::default();
        for i in &in_window {
            if let Some(t) = i.target_id {
                *top.entry(t).or_default() += 1;
            }
        }
        let mut top_monitors: Vec<MonitorIncidentCount> = top
            .into_iter()
            .map(|(target_id, count)| MonitorIncidentCount { target_id, count })
            .collect();
        top_monitors.sort_by(|a, b| b.count.cmp(&a.count).then(a.target_id.cmp(&b.target_id)));
        top_monitors.truncate(10);

        Ok(IncidentMetrics {
            window_days,
            total: in_window.len() as u64,
            mtta_secs: mean(&mtta),
            mttr_secs: mean(&mttr),
            by_severity: tally(&|i| i.severity.as_db_str().to_string()),
            by_state: tally(&|i| i.state.as_db_str().to_string()),
            auto_resolved: in_window
                .iter()
                .filter(|i| i.state == IncidentState::Resolved && i.resolved_by.is_none())
                .count() as u64,
            human_resolved: in_window
                .iter()
                .filter(|i| i.state == IncidentState::Resolved && i.resolved_by.is_some())
                .count() as u64,
            top_monitors,
        })
    }

    async fn declare(
        &self,
        _org: OrgId,
        new: NewManualIncident,
        actor: Actor,
    ) -> Result<OpsIncident> {
        if new.target_id.is_some() && !new.status_page_ids.is_empty() {
            return Err(pages_with_monitor());
        }
        let now = Utc::now();
        let inc = OpsIncident {
            id: Uuid::now_v7(),
            target_id: new.target_id,
            title: new.title,
            state: IncidentState::Triggered,
            severity: new.severity,
            urgency: new.urgency,
            origin: IncidentOrigin::Manual,
            visibility: IncidentVisibility::Internal,
            paging_enabled: new.notify,
            counts_as_downtime: new.counts_as_downtime,
            started_at: now,
            ended_at: None,
            acknowledged_at: None,
            acknowledged_by: None,
            assigned_to: None,
            resolved_by: None,
            escalation_policy_id: None,
            escalation_level: 0,
            escalation_round: 0,
            next_escalation_at: None,
            check_count: 0,
            error_sample: None,
            regions_down: Vec::new(),
            regions_up: Vec::new(),
            created_at: now,
            updated_at: now,
        };
        let mut g = self.inner.lock();
        g.incidents.push(inc.clone());
        g.status_pages.insert(inc.id, new.status_page_ids);
        Self::push_event(&mut g, inc.id, IncidentEventKind::Triggered, actor, None);
        Ok(inc)
    }

    async fn acknowledge(
        &self,
        _org: OrgId,
        id: Uuid,
        actor: Actor,
        note: Option<String>,
        expect_generation: Option<i64>,
    ) -> Result<LifecycleOutcome> {
        Ok(self.apply(
            id,
            IncidentTransition::Acknowledge,
            IncidentEventKind::Acknowledged,
            actor,
            note,
            expect_generation,
            |i| {
                i.state = IncidentState::Acknowledged;
                i.acknowledged_at.get_or_insert_with(Utc::now);
                if i.acknowledged_by.is_none() {
                    i.acknowledged_by = actor.user_id();
                }
                i.next_escalation_at = None;
            },
        ))
    }

    async fn resolve(
        &self,
        _org: OrgId,
        id: Uuid,
        actor: Actor,
        note: Option<String>,
    ) -> Result<LifecycleOutcome> {
        Ok(self.apply(
            id,
            IncidentTransition::Resolve,
            IncidentEventKind::Resolved,
            actor,
            note,
            None,
            |i| {
                i.state = IncidentState::Resolved;
                i.ended_at.get_or_insert_with(Utc::now);
                i.resolved_by = actor.user_id();
                i.next_escalation_at = None;
            },
        ))
    }

    async fn generation(&self, _org: OrgId, id: Uuid) -> Result<Option<i64>> {
        Ok(self.generation_of(id))
    }

    async fn auto_resolve(&self, _org: OrgId, id: Uuid) -> Result<LifecycleOutcome> {
        Ok(self.apply(
            id,
            IncidentTransition::AutoResolve,
            IncidentEventKind::Resolved,
            Actor::System,
            None,
            None,
            |i| {
                i.state = IncidentState::Resolved;
                i.ended_at.get_or_insert_with(Utc::now);
                i.resolved_by = None;
                i.next_escalation_at = None;
            },
        ))
    }

    async fn reopen(
        &self,
        _org: OrgId,
        id: Uuid,
        actor: Actor,
        note: Option<String>,
    ) -> Result<LifecycleOutcome> {
        Ok(self.apply(
            id,
            IncidentTransition::Reopen,
            IncidentEventKind::Reopened,
            actor,
            note,
            None,
            |i| {
                i.state = IncidentState::Triggered;
                i.ended_at = None;
                i.resolved_by = None;
                i.acknowledged_at = None;
                i.acknowledged_by = None;
                i.escalation_level = 0;
                i.escalation_round = 0;
            },
        ))
    }

    async fn assign(
        &self,
        _org: OrgId,
        id: Uuid,
        assignee: Option<UserId>,
        actor: Actor,
    ) -> Result<Option<OpsIncident>> {
        let mut g = self.inner.lock();
        let Some(idx) = g.incidents.iter().position(|i| i.id == id) else {
            return Ok(None);
        };
        g.incidents[idx].assigned_to = assignee;
        g.incidents[idx].updated_at = Utc::now();
        let updated = g.incidents[idx].clone();
        let kind = if assignee.is_some() {
            IncidentEventKind::Assigned
        } else {
            IncidentEventKind::Unassigned
        };
        Self::push_event(&mut g, id, kind, actor, None);
        Ok(Some(updated))
    }

    async fn publish(
        &self,
        _org: OrgId,
        id: Uuid,
        _public_title: Option<String>,
        _public_description: Option<String>,
        status_page_ids: Option<Vec<Uuid>>,
        actor: Actor,
    ) -> Result<Option<OpsIncident>> {
        let mut g = self.inner.lock();
        let Some(idx) = g.incidents.iter().position(|i| i.id == id) else {
            return Ok(None);
        };
        if g.incidents[idx].target_id.is_none() {
            let pages = status_page_ids
                .unwrap_or_else(|| g.status_pages.get(&id).cloned().unwrap_or_default());
            if pages.is_empty() {
                return Err(status_page_required());
            }
            g.status_pages.insert(id, pages);
        } else if status_page_ids.is_some_and(|p| !p.is_empty()) {
            return Err(pages_with_monitor());
        }
        g.incidents[idx].visibility = IncidentVisibility::Public;
        g.incidents[idx].updated_at = Utc::now();
        let updated = g.incidents[idx].clone();
        Self::push_event(&mut g, id, IncidentEventKind::Published, actor, None);
        Ok(Some(updated))
    }

    async fn status_pages(&self, _org: OrgId, id: Uuid) -> Result<Vec<Uuid>> {
        Ok(self
            .inner
            .lock()
            .status_pages
            .get(&id)
            .cloned()
            .unwrap_or_default())
    }

    async fn unpublish(&self, _org: OrgId, id: Uuid, actor: Actor) -> Result<Option<OpsIncident>> {
        let mut g = self.inner.lock();
        let Some(idx) = g.incidents.iter().position(|i| i.id == id) else {
            return Ok(None);
        };
        g.incidents[idx].visibility = IncidentVisibility::Internal;
        g.incidents[idx].updated_at = Utc::now();
        let updated = g.incidents[idx].clone();
        Self::push_event(&mut g, id, IncidentEventKind::Unpublished, actor, None);
        Ok(Some(updated))
    }

    async fn add_note(
        &self,
        _org: OrgId,
        id: Uuid,
        actor: Actor,
        message: String,
    ) -> Result<Option<IncidentEvent>> {
        let mut g = self.inner.lock();
        if !g.incidents.iter().any(|i| i.id == id) {
            return Ok(None);
        }
        Self::push_event(&mut g, id, IncidentEventKind::Note, actor, Some(message));
        Ok(g.events.last().cloned())
    }

    async fn timeline(&self, _org: OrgId, id: Uuid) -> Result<Vec<IncidentEvent>> {
        let mut out: Vec<IncidentEvent> = self
            .inner
            .lock()
            .events
            .iter()
            .filter(|e| e.incident_id == id)
            .cloned()
            .collect();
        out.sort_by_key(|e| e.occurred_at);
        out.truncate(INCIDENT_DETAIL_ROW_CAP as usize);
        Ok(out)
    }

    async fn append_event(
        &self,
        _org: OrgId,
        id: Uuid,
        kind: IncidentEventKind,
        actor: Actor,
        message: Option<String>,
    ) -> Result<()> {
        let mut g = self.inner.lock();
        if g.incidents.iter().any(|i| i.id == id) {
            Self::push_event(&mut g, id, kind, actor, message);
        }
        Ok(())
    }

    async fn notifications_for(&self, _org: OrgId, id: Uuid) -> Result<Vec<IncidentNotification>> {
        Ok(self
            .inner
            .lock()
            .notifications
            .iter()
            .filter(|(_, n)| n.incident_id == id)
            .map(|(_, n)| n.clone())
            .collect())
    }

    async fn record_notification(&self, n: NewIncidentNotification) -> Result<Uuid> {
        let id = Uuid::now_v7();
        let row = IncidentNotification {
            id,
            incident_id: n.incident_id,
            escalation_level: n.escalation_level,
            target_user_id: n.target_user_id,
            channel_id: n.channel_id,
            transport: n.transport,
            reason: n.reason,
            status: n.status,
            attempt: n.attempt,
            error: n.error,
            created_at: Utc::now(),
            sent_at: n.sent_at,
            next_attempt_at: None,
            provider_receipt: None,
            acked_at: None,
        };
        self.inner.lock().notifications.push((n.org, row));
        Ok(id)
    }

    async fn pending_notifications(
        &self,
        now: DateTime<Utc>,
        limit: usize,
        max_attempts: i32,
    ) -> Result<Vec<PendingNotification>> {
        let queued_before = now - chrono::Duration::seconds(QUEUED_TAKEOVER_SECS);
        Ok(self
            .inner
            .lock()
            .notifications
            .iter()
            .filter(|(_, n)| {
                let due = match n.status {
                    NotificationStatus::Failed => true,
                    NotificationStatus::Queued => n.created_at < queued_before,
                    _ => false,
                };
                due && n.attempt < max_attempts && n.next_attempt_at.is_none_or(|t| t <= now)
            })
            .take(limit)
            .map(|(org, n)| PendingNotification {
                id: n.id,
                org: *org,
                incident_id: n.incident_id,
                channel_id: n.channel_id,
                transport: n.transport.clone(),
                reason: n.reason,
                attempt: n.attempt,
            })
            .collect())
    }

    async fn mark_notification(
        &self,
        org: OrgId,
        id: Uuid,
        outcome: NotificationOutcome,
    ) -> Result<()> {
        let mut g = self.inner.lock();
        if let Some((_, n)) = g
            .notifications
            .iter_mut()
            .find(|(o, n)| *o == org && n.id == id)
        {
            n.status = outcome.status;
            n.attempt = outcome.attempt;
            n.error = outcome.error;
            n.sent_at = outcome.sent_at;
            n.next_attempt_at = outcome.next_attempt_at;
            if outcome.provider_receipt.is_some() {
                n.provider_receipt = outcome.provider_receipt;
            }
        }
        Ok(())
    }

    async fn due_emergency_acks(&self, limit: usize) -> Result<Vec<EmergencyAck>> {
        let g = self.inner.lock();
        Ok(g.notifications
            .iter()
            .filter(|(_, n)| {
                n.status == NotificationStatus::Sent
                    && n.acked_at.is_none()
                    && n.provider_receipt.is_some()
                    && n.channel_id.is_some()
            })
            .take(limit)
            .map(|(org, n)| EmergencyAck {
                id: n.id,
                org: *org,
                incident_id: n.incident_id,
                channel_id: n.channel_id.expect("filtered some"),
                receipt: n.provider_receipt.clone().expect("filtered some"),
                generation: reopens_before(&g, n.incident_id, n.sent_at.unwrap_or(n.created_at)),
            })
            .collect())
    }

    async fn emergency_acks_for_incident(
        &self,
        org: OrgId,
        incident_id: Uuid,
    ) -> Result<Vec<EmergencyAck>> {
        let g = self.inner.lock();
        Ok(g.notifications
            .iter()
            .filter(|(o, n)| {
                *o == org
                    && n.incident_id == incident_id
                    && n.status == NotificationStatus::Sent
                    && n.acked_at.is_none()
                    && n.provider_receipt.is_some()
                    && n.channel_id.is_some()
            })
            .map(|(o, n)| EmergencyAck {
                id: n.id,
                org: *o,
                incident_id: n.incident_id,
                channel_id: n.channel_id.expect("filtered some"),
                receipt: n.provider_receipt.clone().expect("filtered some"),
                generation: reopens_before(&g, n.incident_id, n.sent_at.unwrap_or(n.created_at)),
            })
            .collect())
    }

    async fn mark_acked(&self, org: OrgId, id: Uuid, acked_at: DateTime<Utc>) -> Result<()> {
        let mut g = self.inner.lock();
        if let Some((_, n)) = g
            .notifications
            .iter_mut()
            .find(|(o, n)| *o == org && n.id == id)
        {
            n.acked_at = Some(acked_at);
        }
        Ok(())
    }

    async fn clear_receipt(&self, org: OrgId, id: Uuid) -> Result<()> {
        let mut g = self.inner.lock();
        if let Some((_, n)) = g
            .notifications
            .iter_mut()
            .find(|(o, n)| *o == org && n.id == id)
        {
            n.provider_receipt = None;
        }
        Ok(())
    }

    async fn begin_escalation(
        &self,
        _org: OrgId,
        id: Uuid,
        policy_id: Uuid,
        level: i32,
        next_at: Option<DateTime<Utc>>,
    ) -> Result<bool> {
        let mut g = self.inner.lock();
        if let Some(inc) = g
            .incidents
            .iter_mut()
            .find(|i| i.id == id && i.state == IncidentState::Triggered)
        {
            inc.escalation_policy_id = Some(policy_id);
            inc.escalation_level = level;
            inc.escalation_round = 0;
            inc.next_escalation_at = next_at;
            return Ok(true);
        }
        Ok(false)
    }

    async fn record_escalation(
        &self,
        _org: OrgId,
        id: Uuid,
        level: i32,
        round: i32,
        next_at: Option<DateTime<Utc>>,
    ) -> Result<bool> {
        let mut g = self.inner.lock();
        if let Some(inc) = g
            .incidents
            .iter_mut()
            .find(|i| i.id == id && i.state == IncidentState::Triggered)
        {
            inc.escalation_level = level;
            inc.escalation_round = round;
            inc.next_escalation_at = next_at;
            return Ok(true);
        }
        Ok(false)
    }

    async fn due_for_escalation(
        &self,
        now: DateTime<Utc>,
        limit: usize,
        lease_secs: i64,
    ) -> Result<Vec<DueIncident>> {
        // Claim-and-lease, mirroring the Pg impl: bump next_escalation_at forward
        // as the rows are returned so a re-scan doesn't re-pick them.
        let lease_to = now + chrono::Duration::seconds(lease_secs.max(1));
        let mut g = self.inner.lock();
        let mut out = Vec::new();
        for inc in g.incidents.iter_mut().filter(|i| {
            i.state == IncidentState::Triggered && i.next_escalation_at.is_some_and(|t| t <= now)
        }) {
            if out.len() >= limit {
                break;
            }
            out.push(DueIncident {
                id: inc.id,
                org: OrgId(Uuid::nil()),
                target_id: inc.target_id,
                escalation_policy_id: inc.escalation_policy_id,
                escalation_level: inc.escalation_level,
                escalation_round: inc.escalation_round,
            });
            inc.next_escalation_at = Some(lease_to);
        }
        Ok(out)
    }

    async fn due_for_reconcile(
        &self,
        window: (DateTime<Utc>, DateTime<Utc>),
        limit: usize,
    ) -> Result<Vec<DueIncident>> {
        let (since, cutoff) = window;
        let g = self.inner.lock();
        Ok(g.incidents
            .iter()
            .filter(|i| {
                i.state == IncidentState::Triggered
                    && i.started_at <= cutoff
                    && i.started_at >= since
                    && i.escalation_policy_id.is_none()
                    && i.next_escalation_at.is_none()
                    && !g.notifications.iter().any(|(_, n)| n.incident_id == i.id)
            })
            .take(limit)
            .map(|i| DueIncident {
                id: i.id,
                org: OrgId(Uuid::nil()),
                target_id: i.target_id,
                escalation_policy_id: i.escalation_policy_id,
                escalation_level: i.escalation_level,
                escalation_round: i.escalation_round,
            })
            .collect())
    }

    // The reminder cadence depends on the per-target `renotify_interval_secs`,
    // which this single-tenant double doesn't hold (no targets table). The scan
    // is exercised against Postgres; engine tests drive the paging step directly.
    async fn due_for_renotify(
        &self,
        _now: DateTime<Utc>,
        _limit: usize,
    ) -> Result<Vec<DueIncident>> {
        Ok(Vec::new())
    }

    async fn bump_renotify_count(&self, _org: OrgId, id: Uuid) -> Result<()> {
        *self.inner.lock().renotify_counts.entry(id).or_default() += 1;
        Ok(())
    }

    /// Whether a window still covers the target is a join this double has no
    /// tables for; the engine's own guard re-checks it before paging.
    async fn due_for_maintenance_release(&self, limit: usize) -> Result<Vec<DueIncident>> {
        let g = self.inner.lock();
        let held_at = |id: Uuid| {
            g.notifications
                .iter()
                .filter(|(_, n)| {
                    n.incident_id == id && n.channel_id.is_none() && n.transport == "maintenance"
                })
                .map(|(_, n)| n.created_at)
                .max()
        };
        let mut due: Vec<_> = g
            .incidents
            .iter()
            .filter(|i| {
                if i.state != IncidentState::Triggered || i.target_id.is_none() {
                    return false;
                }
                let Some(at) = held_at(i.id) else {
                    return false;
                };
                !g.notifications
                    .iter()
                    .any(|(_, n)| n.incident_id == i.id && n.created_at > at)
            })
            .collect();
        due.sort_by_key(|i| held_at(i.id));
        Ok(due
            .into_iter()
            .take(limit)
            .map(|i| DueIncident {
                id: i.id,
                org: OrgId(Uuid::nil()),
                target_id: i.target_id,
                escalation_policy_id: i.escalation_policy_id,
                escalation_level: i.escalation_level,
                escalation_round: i.escalation_round,
            })
            .collect())
    }

    async fn due_for_flap_release(
        &self,
        now: DateTime<Utc>,
        hold: chrono::Duration,
        limit: usize,
    ) -> Result<Vec<DueIncident>> {
        let cutoff = now - hold;
        let g = self.inner.lock();
        Ok(g.incidents
            .iter()
            .filter(|i| {
                if i.state != IncidentState::Triggered {
                    return false;
                }
                let Some(held_at) = g
                    .notifications
                    .iter()
                    .filter(|(_, n)| {
                        n.incident_id == i.id && n.channel_id.is_none() && n.transport == "damped"
                    })
                    .map(|(_, n)| n.created_at)
                    .max()
                else {
                    return false;
                };
                held_at <= cutoff
                    && !g
                        .notifications
                        .iter()
                        .any(|(_, n)| n.incident_id == i.id && n.created_at > held_at)
            })
            .take(limit)
            .map(|i| DueIncident {
                id: i.id,
                org: OrgId(Uuid::nil()),
                target_id: i.target_id,
                escalation_policy_id: i.escalation_policy_id,
                escalation_level: i.escalation_level,
                escalation_round: i.escalation_round,
            })
            .collect())
    }

    async fn flapping_targets(
        &self,
        _org: OrgId,
        since: DateTime<Utc>,
        min_opens: u32,
    ) -> Result<std::collections::HashSet<Uuid>> {
        if min_opens == 0 {
            return Ok(Default::default());
        }
        let g = self.inner.lock();
        let mut counts: std::collections::HashMap<Uuid, u32> = Default::default();
        for i in g
            .incidents
            .iter()
            .filter(|i| i.started_at >= since && i.origin != IncidentOrigin::Manual)
        {
            if let Some(t) = i.target_id {
                *counts.entry(t).or_default() += 1;
            }
        }
        Ok(counts
            .into_iter()
            .filter(|(_, n)| *n >= min_opens)
            .map(|(t, _)| t)
            .collect())
    }

    async fn opens_since(&self, _org: OrgId, target_id: Uuid, since: DateTime<Utc>) -> Result<u32> {
        let g = self.inner.lock();
        let n = g
            .incidents
            .iter()
            .filter(|i| {
                i.target_id == Some(target_id)
                    && i.started_at >= since
                    && i.origin != IncidentOrigin::Manual
            })
            .count();
        Ok(u32::try_from(n).unwrap_or(u32::MAX))
    }
}
