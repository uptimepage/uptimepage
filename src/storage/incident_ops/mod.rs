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

use std::collections::HashMap;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::domain::{
    ActorType, ExternalId, IncidentAcknowledgement, IncidentEvent, IncidentEventKind,
    IncidentMetrics, IncidentNotification, IncidentSeverity, IncidentState, LinkedApp,
    NewIncidentNotification, NewManualIncident, NotificationOutcome, NotificationReason,
    OpsIncident, OrgId, TransitionError, UserId,
};
use crate::error::Result;
pub use crate::storage::group_incidents::Attach;

mod memory;
mod pg;
#[cfg(test)]
mod tests;

pub use memory::InMemoryIncidentOpsStore;
pub use pg::PgIncidentOpsStore;

/// A `queued` row is a first attempt still in flight; the retry sweep takes it
/// over only once it is older than any single delivery can run.
pub const QUEUED_TAKEOVER_SECS: i64 = 120;

/// The `incident_events.kind` values that start a new episode, for
/// `kind IN {EPISODE_EVENT_KINDS}`. [`IncidentEventKind::starts_episode`] in Rust.
pub(crate) const EPISODE_EVENT_KINDS: &str = "('reopened', 'detached')";

/// [`crate::domain::OpsIncident::may_page`] for the incidents row the query
/// names `$i`.
macro_rules! may_page_predicate {
    ($i:literal) => {
        concat!(
            "(",
            $i,
            ".paging_enabled AND (",
            $i,
            ".target_id IS NOT NULL OR (",
            $i,
            ".origin = 'group' AND ",
            $i,
            ".group_id IS NOT NULL)))"
        )
    };
}
pub(crate) use may_page_predicate;

/// Who is performing an action. Maps onto `incident_events.actor_type` +
/// `actor_id`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Actor {
    System,
    User(UserId),
    Mcp(UserId),
    /// A signed acknowledge link. Carries no user: possession is the proof,
    /// and naming a person would be a guess.
    Link,
    /// Pressed in an app that reports who pressed. Names a member only through
    /// an account they linked, never from where the page was sent.
    App(AppPress),
}

/// A press in an app that reports who pressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AppPress {
    pub app: LinkedApp,
    /// Who pressed, as the app knows them, so two people nobody linked are
    /// still two acknowledgements.
    pub sender: ExternalId,
    /// The member who linked that account, when one of this org did.
    pub member: Option<UserId>,
}

impl Actor {
    pub fn actor_type(self) -> ActorType {
        match self {
            Self::System => ActorType::System,
            Self::User(_) => ActorType::User,
            Self::Mcp(_) => ActorType::Mcp,
            Self::Link => ActorType::Link,
            Self::App(press) => press.app.actor_type(),
        }
    }
    pub fn user_id(self) -> Option<UserId> {
        match self {
            Self::System | Self::Link => None,
            Self::User(u) | Self::Mcp(u) => Some(u),
            Self::App(press) => press.member,
        }
    }
    pub fn sender(self) -> Option<ExternalId> {
        match self {
            Self::App(press) => Some(press.sender),
            Self::System | Self::User(_) | Self::Mcp(_) | Self::Link => None,
        }
    }
    /// Whether a note arriving with the action is someone's own words rather
    /// than a label the channel attached.
    pub fn writes_notes(self) -> bool {
        matches!(self, Self::User(_) | Self::Mcp(_))
    }
}

/// Who pressed, kept only on an acknowledgement that names nobody: there it is
/// what tells two people apart, while a named one is told apart by its member.
pub(crate) fn unnamed_sender(actor: Actor) -> Option<ExternalId> {
    actor.sender().filter(|_| actor.user_id().is_none())
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

/// What an acknowledgement did. `listed` is false when the actor had already
/// acknowledged this episode, or the transition never ran.
#[derive(Debug, Clone)]
pub struct Acknowledged {
    pub outcome: LifecycleOutcome,
    pub listed: bool,
}

/// Filter for the operator incident console.
#[derive(Debug, Clone, Default)]
pub struct IncidentOpsFilter {
    pub state: Option<IncidentState>,
    pub severity: Option<IncidentSeverity>,
    /// Restrict to incidents owned by this user (the "assigned to me" view).
    pub assignee: Option<UserId>,
    /// Free-text match over incident title and monitor or group name
    /// (case-insensitive).
    pub query: Option<String>,
    /// Leave out member incidents of a group; they are listed under it.
    pub top_level: bool,
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

/// An escalation step [`IncidentOpsStore::due_for_escalation`] claimed.
/// `claim` is the timer value the claim leased it with. The step is still the
/// caller's to take only while the timer holds it: an ack or a resolve clears
/// the timer, and a reopen's first page sets its own.
#[derive(Debug, Clone)]
pub struct ClaimedEscalation {
    pub due: DueIncident,
    pub claim: DateTime<Utc>,
    /// The episode the step was claimed in. Its pages are that episode's,
    /// whatever happens to the incident while it is taken.
    pub episode: i64,
}

/// One page's rows, recorded together before any of them is sent, with the
/// bookkeeping the page settles. A crash after the commit leaves every row
/// `queued` for the retry sweep to send; one before it leaves nothing.
#[derive(Debug, Clone)]
pub struct FanOut {
    pub org: OrgId,
    pub incident_id: Uuid,
    /// The episode the page was decided for. Nothing is recorded once the
    /// incident has reopened past it.
    pub episode: i64,
    pub rows: Vec<NewIncidentNotification>,
    /// Timeline entries written with the rows, as the system.
    pub events: Vec<(IncidentEventKind, String)>,
    pub kind: FanOutKind,
}

#[derive(Debug, Clone, Copy)]
pub enum FanOutKind {
    /// The first page an episode owes, arming the ladder when a policy
    /// applies, or what stands in for it (a hold, nobody to page). Records
    /// once: only while the page is still owed and nothing of the episode is
    /// recorded yet, and it settles what is owed. Its rows record while the
    /// incident is open, taken or not; the ladder arms only while triggered.
    Opening(Option<Arming>),
    /// A held first page let go, as [`Self::Opening`] arms and records but
    /// owing nothing: the hold settled that. Refused while a page is owed: a
    /// hold that meets one is a past episode's.
    Release(Option<Arming>),
    /// The rung claimed at `claim`, recorded only while that claim holds and
    /// the monitor is not back up.
    Rung {
        claim: DateTime<Utc>,
        level: i32,
        round: i32,
        next_at: Option<DateTime<Utc>>,
    },
    /// An outage reminder, recorded only while the incident is triggered, its
    /// monitor is not back up, and its episode's first page is recorded.
    Reminder,
    /// The members of a group incident down since its last word about them,
    /// to the channels its episode reached. Records only while one is due,
    /// judged under the group's lock: the group alerts again, the first word
    /// went out [`MEMBER_NOTICE_WINDOW_SECS`] or more ago, and members joined
    /// since. When every one of them is back up it moves past them and
    /// records no rows.
    MemberDigest,
}

/// At most one member-down digest per group incident in this long.
pub const MEMBER_NOTICE_WINDOW_SECS: i64 = 60;

/// A group incident with a member-down digest due.
#[derive(Debug, Clone)]
pub struct DueDigest {
    pub id: Uuid,
    pub org: OrgId,
}

/// An open incident whose episode still owes its first page.
#[derive(Debug, Clone)]
pub struct OwedPage {
    pub id: Uuid,
    pub org: OrgId,
    /// Since when, as read; what [`IncidentOpsStore::requeue_owed_page`]
    /// matches on.
    pub owed_at: DateTime<Utc>,
}

/// The first rung of a policy, as an opening arms it.
#[derive(Debug, Clone, Copy)]
pub struct Arming {
    pub policy_id: Uuid,
    pub level: i32,
    pub next_at: Option<DateTime<Utc>>,
}

/// An ended incident whose closing notice this caller has taken. `claim` is
/// the lease it holds, and settling with it clears only that lease.
#[derive(Debug, Clone)]
pub struct ClosingNotice {
    pub id: Uuid,
    pub org: OrgId,
    pub claim: DateTime<Utc>,
}

/// One closing-notice scan: the notices it took, and how many it withdrew
/// unsent because their incident ended before the window.
#[derive(Debug, Clone, Default)]
pub struct DueClosingNotices {
    pub claimed: Vec<ClosingNotice>,
    pub expired: usize,
}

pub(crate) fn pages_with_monitor() -> crate::error::AppError {
    crate::error::AppError::bad_request_field(
        crate::error::codes::INCIDENT_STATUS_PAGES_WITH_MONITOR,
        "an incident with a monitor appears on the status pages carrying that monitor",
        "status_page_ids",
    )
}

/// Reopening would leave an incident open that nothing can page about or
/// close: its monitor is gone.
pub(crate) fn monitor_deleted() -> crate::error::AppError {
    crate::error::AppError::conflict(
        crate::error::codes::INCIDENT_MONITOR_DELETED,
        "this incident's monitor was deleted, so it cannot be reopened",
    )
}

/// Publishing an incident that ended with its monitor anywhere but the pages
/// that showed the monitor.
pub(crate) fn pages_beyond_deleted_monitor() -> crate::error::AppError {
    crate::error::AppError::conflict(
        crate::error::codes::INCIDENT_MONITOR_DELETED,
        "this incident's monitor was deleted, so it can only be shown on the pages that showed the monitor",
    )
}

/// A member of a group incident is acted on through the group incident.
pub(crate) fn use_group_incident(group_incident: Uuid) -> crate::error::AppError {
    crate::error::AppError::conflict(
        crate::error::codes::USE_GROUP_INCIDENT,
        format!("this incident is part of group incident {group_incident}; act on that one"),
    )
}

/// A group incident follows its members, so nothing would close it again.
pub(crate) fn group_incident_no_reopen() -> crate::error::AppError {
    crate::error::AppError::conflict(
        crate::error::codes::GROUP_INCIDENT_NO_REOPEN,
        "a group incident cannot be reopened; the next member failure opens a new one",
    )
}

/// A group incident pages responders; its members are what status pages show.
pub(crate) fn group_incident_internal() -> crate::error::AppError {
    crate::error::AppError::conflict(
        crate::error::codes::GROUP_INCIDENT_INTERNAL,
        "a group incident stays internal; its members' incidents are the public ones",
    )
}

/// Ack, resolve and reopen refused on a member of a group incident, and reopen
/// on a group incident.
pub(crate) fn group_refusal(
    incident: &OpsIncident,
    transition: crate::domain::IncidentTransition,
) -> Option<crate::error::AppError> {
    if let Some(group_incident) = incident.group.group_incident_id {
        return Some(use_group_incident(group_incident));
    }
    (incident.origin == crate::domain::IncidentOrigin::Group
        && transition == crate::domain::IncidentTransition::Reopen)
        .then(group_incident_no_reopen)
}

pub(crate) fn unknown_status_pages(missing: usize) -> crate::error::AppError {
    crate::error::AppError::bad_request_field(
        crate::error::codes::INVALID_STATUS_PAGE_ID,
        format!("{missing} status page id(s) do not exist"),
        "status_page_ids",
    )
}

pub(crate) fn status_page_required() -> crate::error::AppError {
    crate::error::AppError::bad_request_field(
        crate::error::codes::INCIDENT_STATUS_PAGE_REQUIRED,
        "an incident with no monitor needs at least one status page to appear on",
        "status_page_ids",
    )
}

/// What a public update says, and whether the platform wrote it because the
/// operator gave no words of their own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateText {
    pub message: String,
    pub generated: bool,
}

impl UpdateText {
    /// The operator's words when they gave any, else the platform's `fallback`.
    pub fn or_default(written: Option<&str>, fallback: &str) -> Self {
        match written.map(str::trim).filter(|s| !s.is_empty()) {
            Some(text) => Self {
                message: text.to_string(),
                generated: false,
            },
            None => Self {
                message: fallback.to_string(),
                generated: true,
            },
        }
    }
}

/// Body of the update that publishing an incident posts for it. Subscribers are
/// notified per update, so publishing without one would reach nobody; shared so
/// a confirmation prompt can show the text before it goes out.
pub fn opening_update_message(title: Option<&str>, description: Option<&str>) -> UpdateText {
    let non_blank = |s: &&str| !s.trim().is_empty();
    UpdateText::or_default(
        description.filter(non_blank).or(title.filter(non_blank)),
        "We are investigating this incident.",
    )
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
    /// The member incidents of each group incident in `group_incidents`, in
    /// the order they joined, at most `each` per group incident.
    async fn group_members(
        &self,
        org: OrgId,
        group_incidents: &[Uuid],
        each: usize,
    ) -> Result<Vec<OpsIncident>>;
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
    ) -> Result<Acknowledged>;

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
    /// Resolve by a press on an alert, pinned to the episode it was about like
    /// [`Self::acknowledge`]. An incident already closed is refused as an
    /// illegal transition rather than recorded again.
    async fn resolve_episode(
        &self,
        org: OrgId,
        id: Uuid,
        actor: Actor,
        episode: i64,
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
    ///
    /// `status_page_ids` replaces a monitor-less incident's pages (`None`
    /// keeps them). Refused when the incident has a monitor, or when a
    /// monitor-less one would end up on no page at all.
    async fn publish(
        &self,
        org: OrgId,
        id: Uuid,
        public_title: Option<String>,
        public_description: Option<String>,
        status_page_ids: Option<Vec<Uuid>>,
        actor: Actor,
    ) -> Result<Option<OpsIncident>>;
    /// Pages a monitor-less incident was published to, empty for one with a
    /// monitor.
    async fn status_pages(&self, org: OrgId, id: Uuid) -> Result<Vec<Uuid>>;
    /// Pages that kept a monitor's incident in their history after the
    /// monitor left them; they show it too, so a change to it must reach them.
    async fn kept_on_pages(&self, org: OrgId, id: Uuid) -> Result<Vec<Uuid>>;
    /// Incidents closed by deleting these monitors, whose responders are told
    /// it closed.
    async fn closed_with_monitors(&self, org: OrgId, monitors: &[Uuid]) -> Result<Vec<Uuid>>;
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
    /// Everyone who acknowledged each incident's current episode, in the order
    /// they did, keyed by incident. Incidents nobody has acknowledged are absent.
    async fn acknowledgements(
        &self,
        org: OrgId,
        ids: &[Uuid],
    ) -> Result<HashMap<Uuid, Vec<IncidentAcknowledgement>>>;
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
    /// Record `f` whole, under the incident's lock: its rows, its timeline
    /// entries and its bookkeeping. `None`, with nothing written, once the page
    /// no longer applies: the incident is gone or resolved, has no monitor or
    /// no paging, has reopened past `f.episode`, or the kind's own guard fails.
    /// Row ids come back in the order given. A group incident's rows also
    /// store the members still down, read under the group's lock, and the
    /// first of its pages to reach anyone marks them told.
    async fn record_fan_out(&self, f: FanOut) -> Result<Option<Vec<Uuid>>>;
    /// The members a group page's row named, by name, in the order they
    /// joined. Empty for any other row.
    async fn notice_members(&self, org: OrgId, notification_id: Uuid) -> Result<Vec<String>>;
    /// A routed member incident's opening: it joins its group's open incident,
    /// or opens one owed its first page.
    async fn attach_member(&self, org: OrgId, member: Uuid) -> Result<Attach>;
    /// Cross-org open group incidents whose member-down digest is due, as
    /// [`FanOutKind::MemberDigest`] judges it. Longest waiting first.
    async fn due_member_digests(&self, limit: usize) -> Result<Vec<DueDigest>>;
    /// Whether this group incident's digest is due now, for the cheap check
    /// before planning one.
    async fn member_digest_due(&self, org: OrgId, group_incident: Uuid) -> Result<bool>;
    /// A group incident pages while one of its members still down is watched:
    /// on, held by no plan and in no maintenance window that holds alerts.
    async fn group_watched(&self, org: OrgId, group_incident: Uuid) -> Result<bool>;
    /// Cross-org failed pages still under the attempt cap whose backoff has
    /// elapsed (`next_attempt_at` null or `<= now`), soonest-due first — the
    /// engine's retry sweep. A `queued` row counts once older than
    /// [`QUEUED_TAKEOVER_SECS`], whatever the cap: its attempt never ended.
    async fn pending_notifications(
        &self,
        now: DateTime<Utc>,
        limit: usize,
        max_attempts: i32,
    ) -> Result<Vec<PendingNotification>>;
    /// Update a paging row after a delivery attempt. `next_attempt_at` schedules
    /// the next retry (backoff) or clears it once sent/suppressed/exhausted.
    /// A delivery always lands and nothing lands on one: a page that reached
    /// its channel is done, whatever a takeover running beside it reports.
    /// Otherwise an outcome lands only if no later attempt has written one, so
    /// a first attempt stalled past the takeover never overwrites the retry's.
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
    /// Advance escalation bookkeeping during the sweep when no page goes out,
    /// guarded on `state = 'triggered'` so a concurrent ack/resolve (which
    /// clears the timer) is never overwritten; `next_at = None` stops further
    /// escalation (exhausted). Returns whether a row changed.
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
    ) -> Result<Vec<ClaimedEscalation>>;
    /// Cross-org open incidents whose episode has owed its first page since
    /// `cutoff` or earlier, whatever their age, on a monitor that is watched
    /// (not paused, not held by its plan) and with paging on. Longest owed
    /// first.
    async fn due_for_owed_page(&self, cutoff: DateTime<Utc>, limit: usize)
    -> Result<Vec<OwedPage>>;
    /// Move a page still owed since `seen` to the back of the queue, so one
    /// that keeps failing never holds up the ones behind it. A page recorded
    /// since, or owed again by a reopen, is left alone.
    async fn requeue_owed_page(&self, org: OrgId, id: Uuid, seen: DateTime<Utc>) -> Result<()>;
    /// Every close records, in the statement that ends the incident, that the
    /// responders it paged are owed the notice that it ended; a reopen
    /// withdraws it. This takes that notice for `lease_secs`. `None` when
    /// nothing is owed (never closed, reopened since, already sent) or another
    /// sender holds it.
    async fn claim_closing_notice(
        &self,
        org: OrgId,
        id: Uuid,
        lease_secs: i64,
    ) -> Result<Option<DateTime<Utc>>>;
    /// Cross-org closing notices owed for at least `grace_secs`, the ones whose
    /// signal was lost, each claimed for `lease_secs` under `FOR UPDATE SKIP
    /// LOCKED` so a second engine instance never takes the same one. A notice
    /// for an incident that ended more than `window_secs` ago is withdrawn
    /// unsent. The oldest go first when more than `limit` are owed.
    async fn due_closing_notices(
        &self,
        grace_secs: i64,
        window_secs: i64,
        lease_secs: i64,
        limit: usize,
    ) -> Result<DueClosingNotices>;
    /// Persist one row of the closing notice held under `claim`, only while
    /// that claim still holds, and renew its lease for `lease_secs`. Returns
    /// the row id and the renewed claim; `None`, recording nothing, once the
    /// claim is lost: another sender took the notice after this lease ran out,
    /// or a reopen withdrew it. One statement does both, so whoever takes the
    /// notice next sees every row this sender recorded.
    async fn record_closing_notification(
        &self,
        n: NewIncidentNotification,
        claim: DateTime<Utc>,
        lease_secs: i64,
    ) -> Result<Option<(Uuid, DateTime<Utc>)>>;
    /// The notice taken under `claim` has been dealt with. One a later close
    /// owes, or another sender took once this lease ran out, stays.
    async fn settle_closing_notice(&self, org: OrgId, id: Uuid, claim: DateTime<Utc>)
    -> Result<()>;
    /// Cross-org open, unacknowledged incidents whose monitor wants an outage
    /// reminder (`renotify_interval_secs > 0`) and whose last page attempt is
    /// older than that interval doubled once per reminder already sent, and
    /// which are not mid-escalation
    /// (`next_escalation_at IS NULL` — an active ladder drives its own cadence).
    /// The engine re-pages the channels already notified this episode. Oldest
    /// last-page first.
    async fn due_for_renotify(&self, now: DateTime<Utc>, limit: usize) -> Result<Vec<DueIncident>>;

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
