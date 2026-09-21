//! Operational incident lifecycle storage.
//!
//! Separate from [`crate::storage::incidents`] (public narration) and from
//! `public_status::incident_writer::IncidentStore` (the auto open/close
//! materialiser): this trait owns the *internal* operational surface — the
//! state machine (acknowledge / assign / resolve / reopen), the internal
//! activity log, and the read model that backs the operator console.
//!
//! Every method takes the caller's `org`. The Postgres store filters `org_id`
//! in every statement, so a caller cannot reach another tenant's rows; the
//! in-memory store is a single-tenant test double and matches on id alone.
//! Postgres state transitions run under a per-incident advisory lock so
//! concurrent ack/resolve/escalate cannot race the machine.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use subtle::ConstantTimeEq;
use uuid::Uuid;

use crate::domain::{
    ActorType, IncidentEvent, IncidentEventKind, IncidentMetrics, IncidentNotification,
    IncidentSeverity, IncidentState, NewIncidentNotification, NewManualIncident,
    NotificationOutcome, NotificationReason, OpsIncident, OrgId, TransitionError, UserId,
};
use crate::error::Result;

mod memory;
mod pg;
#[cfg(test)]
mod tests;

pub use memory::InMemoryIncidentOpsStore;
pub use pg::PgIncidentOpsStore;

/// Bounds a leaked link: unlike a mailed one this rides in a push payload that
/// may sit on someone else's server.
pub const ACK_LINK_TTL_SECS: i64 = 7 * 24 * 60 * 60;

/// A `queued` row is a first attempt still in flight; the retry sweep takes it
/// over only once it is older than any single delivery can run.
pub const QUEUED_TAKEOVER_SECS: i64 = 120;

/// Proof for the public acknowledge link, bound to one outage on one incident.
/// Reproduced at verify time, nothing persisted.
pub fn incident_ack_token(
    secret: &str,
    org: OrgId,
    incident_id: Uuid,
    channel_id: Uuid,
    generation: i64,
    expires_at: i64,
) -> String {
    let gen_exp = format!("{generation}:{expires_at}");
    crate::security::mac::hmac_sha256_hex(
        secret.as_bytes(),
        &[
            org.0.as_bytes(),
            incident_id.as_bytes(),
            channel_id.as_bytes(),
            gen_exp.as_bytes(),
        ],
    )
}

pub fn verify_incident_ack(
    secret: &str,
    org: OrgId,
    incident_id: Uuid,
    channel_id: Uuid,
    generation: i64,
    expires_at: i64,
    presented: &str,
) -> bool {
    incident_ack_token(secret, org, incident_id, channel_id, generation, expires_at)
        .as_bytes()
        .ct_eq(presented.as_bytes())
        .into()
}

/// `None` when the base URL or secret is unset, so no dead link reaches a phone.
pub fn incident_ack_url(
    base_url: &str,
    secret: &str,
    org: OrgId,
    incident_id: Uuid,
    channel_id: Uuid,
    generation: i64,
    now: DateTime<Utc>,
) -> Option<String> {
    let base = base_url.trim_end_matches('/');
    if base.is_empty() || secret.is_empty() {
        return None;
    }
    let exp = now.timestamp() + ACK_LINK_TTL_SECS;
    let mac = incident_ack_token(secret, org, incident_id, channel_id, generation, exp);
    Some(format!(
        "{base}/incident/ack?o={}&i={incident_id}&c={channel_id}&g={generation}&e={exp}&t={mac}",
        org.0
    ))
}

/// Who is performing an action. Maps onto `incident_events.actor_type` +
/// `actor_id`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Actor {
    System,
    User(UserId),
    Mcp(UserId),
    /// A signed acknowledge link, or a push app that acknowledged on its own
    /// side. Carries no user: possession is the proof, and naming a person
    /// would be a guess.
    Link,
}

impl Actor {
    pub fn actor_type(self) -> ActorType {
        match self {
            Self::System => ActorType::System,
            Self::User(_) => ActorType::User,
            Self::Mcp(_) => ActorType::Mcp,
            Self::Link => ActorType::Link,
        }
    }
    pub fn user_id(self) -> Option<UserId> {
        match self {
            Self::System | Self::Link => None,
            Self::User(u) | Self::Mcp(u) => Some(u),
        }
    }
}

/// Result of a lifecycle mutation: distinguishes a missing incident from an
/// illegal transition (which the API layer maps to 409, not 404).
#[derive(Debug, Clone)]
pub enum LifecycleOutcome {
    Updated(Box<OpsIncident>),
    NotFound,
    IllegalTransition(TransitionError),
    /// Aimed at an episode the incident has already left: a page from before a
    /// resolve/reopen must not silence the outage that followed.
    Stale,
}

/// Filter for the operator incident console.
#[derive(Debug, Clone, Default)]
pub struct IncidentOpsFilter {
    pub state: Option<IncidentState>,
    pub severity: Option<IncidentSeverity>,
    /// Restrict to incidents owned by this user (the "assigned to me" view).
    pub assignee: Option<UserId>,
    /// Free-text match over incident title and monitor name (case-insensitive).
    pub query: Option<String>,
    pub sort: IncidentSort,
    pub limit: Option<usize>,
    pub offset: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum IncidentSort {
    #[default]
    Recent,
    Oldest,
    Severity,
}

impl IncidentSort {
    pub fn from_key(key: &str) -> Self {
        match key {
            "oldest" => Self::Oldest,
            "severity" => Self::Severity,
            _ => Self::Recent,
        }
    }
    fn order_sql(self) -> &'static str {
        match self {
            Self::Recent => "started_at DESC",
            Self::Oldest => "started_at ASC",
            Self::Severity => {
                "CASE severity WHEN 'critical' THEN 3 WHEN 'major' THEN 2 ELSE 1 END DESC, \
                 started_at DESC"
            }
        }
    }
}

/// Per-state incident tallies for the console filter tabs, under the active
/// severity/assignee filter (state itself excluded so each tab shows its own
/// total).
#[derive(Debug, Clone, Copy, Default)]
pub struct IncidentStateCounts {
    pub triggered: usize,
    pub acknowledged: usize,
    pub resolved: usize,
}

impl IncidentStateCounts {
    pub fn total(&self) -> usize {
        self.triggered + self.acknowledged + self.resolved
    }

    pub fn for_state(&self, state: Option<IncidentState>) -> usize {
        match state {
            Some(IncidentState::Triggered) => self.triggered,
            Some(IncidentState::Acknowledged) => self.acknowledged,
            Some(IncidentState::Resolved) => self.resolved,
            None => self.total(),
        }
    }
}

/// A failed paging row the escalation engine should re-attempt. Carries the
/// owning `org` (absent from the public [`IncidentNotification`]) so the engine
/// can re-resolve the incident, monitor, and channel to rebuild the message.
#[derive(Debug, Clone)]
pub struct PendingNotification {
    pub id: Uuid,
    pub org: OrgId,
    pub incident_id: Uuid,
    pub channel_id: Option<Uuid>,
    pub transport: String,
    pub reason: NotificationReason,
    pub attempt: i32,
}

/// A sent emergency page still awaiting acknowledgement, carrying the channel
/// whose application token polls/cancels its Pushover receipt.
#[derive(Debug, Clone)]
pub struct EmergencyAck {
    pub id: Uuid,
    pub org: OrgId,
    pub incident_id: Uuid,
    pub channel_id: Uuid,
    pub receipt: String,
    /// Episode this page went out for. A reopen tries to cancel the receipt,
    /// but a dropped signal or a failed cancel both leave it outstanding, so
    /// the ack cannot lean on that.
    pub generation: i64,
}

/// A triggered incident whose escalation timer is due. Carries the owning
/// `org` (cross-org scan) plus the bookkeeping the engine needs to re-resolve
/// the policy and compute the next rung.
#[derive(Debug, Clone)]
pub struct DueIncident {
    pub id: Uuid,
    pub org: OrgId,
    pub target_id: Option<Uuid>,
    pub escalation_policy_id: Option<Uuid>,
    pub escalation_level: i32,
    pub escalation_round: i32,
}

/// Body of the update that publishing an incident posts for it. Subscribers are
/// notified per update, so publishing without one would reach nobody; shared so
/// a confirmation prompt can show the text before it goes out.
pub fn opening_update_message(title: Option<&str>, description: Option<&str>) -> String {
    let non_blank = |s: &&str| !s.trim().is_empty();
    description
        .filter(non_blank)
        .or(title.filter(non_blank))
        .map(str::trim)
        .unwrap_or("We are investigating this incident.")
        .to_string()
}

/// Public opening line for a monitor-opened incident that lands on a status
/// page; written by the writer's `insert_open` so subscribers hear the outage
/// start, not only its end.
pub const AUTO_OPENED_MESSAGE: &str = "Automatically detected — monitoring checks are failing.";

/// Public closing line for an auto-resolved incident; shared with the writer's
/// `close` path.
pub const AUTO_RESOLVED_MESSAGE: &str =
    "Automatically resolved — monitoring checks have recovered.";

/// Upper bound on per-incident timeline/update rows a detail view renders, so a
/// pathological long-lived incident can't blow up the query or page.
pub const INCIDENT_DETAIL_ROW_CAP: i64 = 500;

#[async_trait]
pub trait IncidentOpsStore: Send + Sync {
    async fn get(&self, org: OrgId, id: Uuid) -> Result<Option<OpsIncident>>;
    async fn list(&self, org: OrgId, filter: IncidentOpsFilter) -> Result<Vec<OpsIncident>>;
    /// Total incidents in `org` matching the filter — for pager "page N of M".
    /// Honours state/severity/assignee (offset/limit ignored).
    async fn count(&self, org: OrgId, filter: &IncidentOpsFilter) -> Result<usize>;
    /// Per-state tallies for the console tabs, under the same severity/assignee
    /// filter (the filter's own `state` is ignored — every state is counted).
    async fn counts_by_state(
        &self,
        org: OrgId,
        filter: &IncidentOpsFilter,
    ) -> Result<IncidentStateCounts>;
    /// Aggregate incident reporting (MTTA/MTTR, counts, noisiest monitors) over
    /// a trailing window of `window_days`, scoped to `org`.
    async fn metrics(&self, org: OrgId, window_days: u32) -> Result<IncidentMetrics>;
    async fn declare(
        &self,
        org: OrgId,
        new: NewManualIncident,
        actor: Actor,
    ) -> Result<OpsIncident>;
    /// `expect_generation` pins the acknowledgement to one episode: `None` for
    /// a human at the console, who sees the incident as it is now; `Some` for a
    /// link or receipt minted by a page, which may predate a reopen.
    async fn acknowledge(
        &self,
        org: OrgId,
        id: Uuid,
        actor: Actor,
        note: Option<String>,
        expect_generation: Option<i64>,
    ) -> Result<LifecycleOutcome>;

    /// How many times the incident has reopened — its episode number. `None`
    /// when the incident is gone.
    async fn generation(&self, org: OrgId, id: Uuid) -> Result<Option<i64>>;
    /// Manual resolve by a human (`resolved_by` = the actor's user).
    async fn resolve(
        &self,
        org: OrgId,
        id: Uuid,
        actor: Actor,
        note: Option<String>,
    ) -> Result<LifecycleOutcome>;
    /// Recovery detected by the writer (`resolved_by` = NULL, actor = system).
    async fn auto_resolve(&self, org: OrgId, id: Uuid) -> Result<LifecycleOutcome>;
    async fn reopen(
        &self,
        org: OrgId,
        id: Uuid,
        actor: Actor,
        note: Option<String>,
    ) -> Result<LifecycleOutcome>;
    async fn assign(
        &self,
        org: OrgId,
        id: Uuid,
        assignee: Option<UserId>,
        actor: Actor,
    ) -> Result<Option<OpsIncident>>;
    /// Flip an incident to public visibility, optionally seeding the public
    /// narration (a `None` field leaves the stored copy untouched). Logs a
    /// `published` event. `None` ⇒ no such incident in `org`.
    async fn publish(
        &self,
        org: OrgId,
        id: Uuid,
        public_title: Option<String>,
        public_description: Option<String>,
        actor: Actor,
    ) -> Result<Option<OpsIncident>>;
    /// Flip an incident back to internal visibility. Logs an `unpublished`
    /// event. `None` ⇒ no such incident in `org`.
    async fn unpublish(&self, org: OrgId, id: Uuid, actor: Actor) -> Result<Option<OpsIncident>>;
    async fn add_note(
        &self,
        org: OrgId,
        id: Uuid,
        actor: Actor,
        message: String,
    ) -> Result<Option<IncidentEvent>>;
    async fn timeline(&self, org: OrgId, id: Uuid) -> Result<Vec<IncidentEvent>>;
    /// Append one internal timeline entry outside a lifecycle transition (e.g.
    /// the escalation engine logging a `notified`/`escalated` event).
    async fn append_event(
        &self,
        org: OrgId,
        id: Uuid,
        kind: IncidentEventKind,
        actor: Actor,
        message: Option<String>,
    ) -> Result<()>;
    /// Every paging-log row for an incident — the engine reads these to dedup
    /// (never page the same channel+reason twice) before sending.
    async fn notifications_for(&self, org: OrgId, id: Uuid) -> Result<Vec<IncidentNotification>>;
    /// Persist one paging attempt. Returns the new row id.
    async fn record_notification(&self, n: NewIncidentNotification) -> Result<Uuid>;
    /// Cross-org failed pages still under the attempt cap whose backoff has
    /// elapsed (`next_attempt_at` null or `<= now`), soonest-due first — the
    /// engine's retry sweep. A `queued` row counts only after
    /// [`QUEUED_TAKEOVER_SECS`].
    async fn pending_notifications(
        &self,
        now: DateTime<Utc>,
        limit: usize,
        max_attempts: i32,
    ) -> Result<Vec<PendingNotification>>;
    /// Update a paging row after a delivery attempt. `next_attempt_at` schedules
    /// the next retry (backoff) or clears it once sent/suppressed/exhausted.
    /// `org`-scoped to keep the tenant-isolation invariant even though ids today
    /// come from the engine.
    async fn mark_notification(
        &self,
        org: OrgId,
        id: Uuid,
        outcome: NotificationOutcome,
    ) -> Result<()>;
    /// Cross-org sent emergency pages awaiting acknowledgement — the ack-poll
    /// sweep. Only rows with a receipt and a still-present channel.
    async fn due_emergency_acks(&self, limit: usize) -> Result<Vec<EmergencyAck>>;
    /// Outstanding emergency pages for one incident — the resolve path cancels
    /// these so a resolved incident stops repeating.
    async fn emergency_acks_for_incident(
        &self,
        org: OrgId,
        incident_id: Uuid,
    ) -> Result<Vec<EmergencyAck>>;
    /// Stamp the acknowledgement time and stop polling the row.
    async fn mark_acked(&self, org: OrgId, id: Uuid, acked_at: DateTime<Utc>) -> Result<()>;
    /// Drop the receipt so the row leaves the poll set (cancelled or expired).
    async fn clear_receipt(&self, org: OrgId, id: Uuid) -> Result<()>;
    /// Start escalation on a freshly-opened incident: stamp the resolved
    /// policy, set the first level + round 0, and arm `next_escalation_at`.
    /// Guarded on `state = 'triggered'` so a concurrent ack/resolve (which
    /// clears the timer) is never overwritten. Returns whether a row changed.
    async fn begin_escalation(
        &self,
        org: OrgId,
        id: Uuid,
        policy_id: Uuid,
        level: i32,
        next_at: Option<DateTime<Utc>>,
    ) -> Result<bool>;
    /// Advance escalation bookkeeping during the sweep. Same `triggered` guard
    /// as [`Self::begin_escalation`]; `next_at = None` stops further escalation
    /// (exhausted). Returns whether a row changed.
    async fn record_escalation(
        &self,
        org: OrgId,
        id: Uuid,
        level: i32,
        round: i32,
        next_at: Option<DateTime<Utc>>,
    ) -> Result<bool>;
    /// Atomically CLAIM cross-org triggered incidents whose escalation timer has
    /// elapsed, soonest first — the engine's escalation sweep. The claim pushes
    /// `next_escalation_at` forward by `lease_secs` under `FOR UPDATE SKIP
    /// LOCKED`, so a second engine instance (multi-box) never grabs the same
    /// rung; the caller then pages and records the real next time. If the caller
    /// dies mid-rung the lease expires and the rung is retried (at-least-once).
    async fn due_for_escalation(
        &self,
        now: DateTime<Utc>,
        limit: usize,
        lease_secs: i64,
    ) -> Result<Vec<DueIncident>>;
    /// Cross-org `triggered` incidents that were opened but never paged and
    /// never armed — no paging-log row, no bound policy, no timer — started
    /// inside `window`, given as `(floor, cutoff)`. These are incidents whose open signal was lost
    /// (e.g. the signal channel saturated); the engine reconciles them by
    /// re-running the open-episode paging. Oldest first.
    async fn due_for_reconcile(
        &self,
        window: (DateTime<Utc>, DateTime<Utc>),
        limit: usize,
    ) -> Result<Vec<DueIncident>>;
    /// Cross-org open, unacknowledged incidents whose monitor wants an outage
    /// reminder (`renotify_interval_secs > 0`) and whose last page attempt is
    /// older than that interval doubled once per reminder already sent, and
    /// which are not mid-escalation
    /// (`next_escalation_at IS NULL` — an active ladder drives its own cadence).
    /// The engine re-pages the channels already notified this episode. Oldest
    /// last-page first.
    async fn due_for_renotify(&self, now: DateTime<Utc>, limit: usize) -> Result<Vec<DueIncident>>;

    /// Widens the incident's next reminder gap. Reset by `reopen`.
    async fn bump_renotify_count(&self, org: OrgId, id: Uuid) -> Result<()>;

    /// Open incidents holding a maintenance marker whose window has since
    /// ended (or stopped suppressing) and that no page has reached. Oldest
    /// hold first.
    async fn due_for_maintenance_release(&self, limit: usize) -> Result<Vec<DueIncident>>;
    /// The flap damper's input. Counts opens, not current state — repeated
    /// fail/recover cycles are exactly what it has to see. Excludes manually
    /// declared incidents.
    async fn opens_since(&self, org: OrgId, target_id: Uuid, since: DateTime<Utc>) -> Result<u32>;
    /// Monitors in `org` that opened at least `min_opens` incidents since
    /// `since`. One aggregate for a whole page render, so the console can mark
    /// a flapping monitor without a per-row query or any stored state.
    async fn flapping_targets(
        &self,
        org: OrgId,
        since: DateTime<Utc>,
        min_opens: u32,
    ) -> Result<std::collections::HashSet<Uuid>>;
    /// Held incidents still open past `hold` — a flap closes well before it,
    /// so anything left has to page despite the monitor's noise. Oldest first.
    async fn due_for_flap_release(
        &self,
        now: DateTime<Utc>,
        hold: chrono::Duration,
        limit: usize,
    ) -> Result<Vec<DueIncident>>;
}
