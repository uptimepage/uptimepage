use chrono::{DateTime, Duration as ChronoDuration, Utc};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use utoipa::ToSchema;
use uuid::Uuid;

use super::org::OrgId;
use super::public::{IncidentSeverity, IncidentStatusPhase, PublicIncidentUpdate};
use super::result::CheckStatus;
use super::user::UserId;

/// A contiguous period during which a target was `down` or `error`. Two
/// consecutive bad checks separated by one `up` count as separate incidents.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct Incident {
    pub id: Uuid,
    /// `None` for an incident an operator declared without naming a monitor.
    #[schema(nullable = true)]
    pub target_id: Option<Uuid>,
    pub started_at: DateTime<Utc>,
    /// `null` if the incident is ongoing.
    #[schema(nullable = true)]
    pub ended_at: Option<DateTime<Utc>>,
    pub status: CheckStatus,
    /// Duration in seconds. `null` while ongoing.
    #[schema(nullable = true)]
    pub duration_secs: Option<u64>,
    pub check_count: u64,
    #[schema(nullable = true)]
    pub error_sample: Option<String>,
    #[serde(default)]
    #[schema(default = "major")]
    pub severity: IncidentSeverity,
    #[serde(default = "default_true")]
    #[schema(default = true)]
    pub counts_as_downtime: bool,
    #[serde(default)]
    #[schema(nullable = true)]
    pub public_title: Option<String>,
    #[serde(default)]
    #[schema(nullable = true)]
    pub public_description: Option<String>,
    #[serde(default)]
    #[schema(nullable = true)]
    pub created_at: Option<DateTime<Utc>>,
    #[serde(default)]
    #[schema(nullable = true)]
    pub updated_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub updates: Vec<PublicIncidentUpdate>,
    /// Empty for a single-region monitor, or a coalesced-stream incident.
    #[serde(default)]
    pub regions_down: Vec<String>,
    #[serde(default)]
    pub regions_up: Vec<String>,
}

impl Incident {
    /// Resolved duration of a CLOSED incident. Returns `None` for ongoing
    /// incidents. Prefers the stored `duration_secs` set by the coalescer;
    /// falls back to `(ended_at - started_at)` when missing. Saturating
    /// cast guards against `u64::MAX` overflowing `i64` (unreachable in
    /// practice; the duration would have to exceed ~292 billion years).
    pub fn closed_duration(&self) -> Option<ChronoDuration> {
        self.ended_at.map(|end| {
            if let Some(s) = self.duration_secs {
                return ChronoDuration::seconds(i64::try_from(s).unwrap_or(i64::MAX));
            }
            (end - self.started_at).max(ChronoDuration::zero())
        })
    }
}

/// Wall-clock duration of an incident at the given `now`. Open-ended
/// incidents clamp to `now`. Used by view layers that want a single
/// "how long has this been going on" number for both closed and
/// ongoing rows. Always non-negative.
pub fn elapsed_at(
    started_at: DateTime<Utc>,
    ended_at: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> ChronoDuration {
    let end = ended_at.unwrap_or(now);
    (end - started_at).max(ChronoDuration::zero())
}

/// Confirmed downtime in seconds: each incident's overlap with `[from, to]`,
/// summed; an ongoing incident is clamped to `now`.
pub fn confirmed_downtime_secs(
    incidents: &[Incident],
    from: DateTime<Utc>,
    to: DateTime<Utc>,
    now: DateTime<Utc>,
) -> i64 {
    incidents
        .iter()
        .filter(|i| i.counts_as_downtime)
        .map(|i| {
            let end = i.ended_at.unwrap_or(now).min(to);
            let start = i.started_at.max(from);
            (end - start).num_seconds().max(0)
        })
        .sum()
}

/// Uptime over a window as `100 * (1 - downtime/window)`, clamped to [0, 100].
pub fn uptime_pct_from_downtime(downtime_secs: i64, window_secs: i64) -> f64 {
    if window_secs <= 0 {
        return 100.0;
    }
    let up = (window_secs - downtime_secs).max(0) as f64;
    (up / window_secs as f64 * 100.0).clamp(0.0, 100.0)
}

/// Amendment to an existing incident: the operator-facing fields (`title`,
/// `severity`, `urgency`) and the customer-facing narration.
///
/// `public_title` and `public_description` use a "double-Option" pattern so
/// callers can distinguish three cases:
///  * field omitted entirely → leave the stored value unchanged
///  * field present with JSON `null` → clear the stored value
///  * field present with a string → set the stored value to that string
///
/// utoipa renders both layers as nullable; the wire shape is identical to a
/// plain `Option<String>` to clients.
#[derive(Debug, Clone, Default, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct IncidentNarrationUpdate {
    #[serde(default, deserialize_with = "double_option")]
    #[schema(nullable = true, value_type = Option<String>)]
    pub public_title: Option<Option<String>>,
    #[serde(default, deserialize_with = "double_option")]
    #[schema(nullable = true, value_type = Option<String>)]
    pub public_description: Option<Option<String>>,
    /// Internal title. Clearing it falls the label back to the monitor name.
    #[serde(default, deserialize_with = "double_option")]
    #[schema(nullable = true, value_type = Option<String>)]
    pub title: Option<Option<String>>,
    #[serde(default)]
    pub severity: Option<IncidentSeverity>,
    #[serde(default)]
    pub urgency: Option<IncidentUrgency>,
    /// Declared incidents only; rejected on a monitor-opened one.
    #[serde(default)]
    pub counts_as_downtime: Option<bool>,
}

/// Lifts the inner `Option<T>` into a `Some(Option<T>)` so missing fields stay
/// `None` (via `#[serde(default)]`) while explicit JSON `null` becomes
/// `Some(None)` (the "clear" intent). Without this, serde's default treats
/// both as `None`, making it impossible to distinguish "leave alone" from
/// "clear to null" on PATCH.
fn double_option<'de, T, D>(d: D) -> std::result::Result<Option<Option<T>>, D::Error>
where
    T: Deserialize<'de>,
    D: Deserializer<'de>,
{
    Option::<T>::deserialize(d).map(Some)
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct NewIncidentUpdate {
    pub phase: IncidentStatusPhase,
    #[schema(min_length = 1, max_length = 2000)]
    pub message: String,
}

/// Coalesces an ordered (ascending by timestamp) stream of check observations
/// into incidents. Each contiguous run of `down`/`error` statuses becomes one
/// `Incident`. Trailing ongoing runs are preserved with `ended_at: None`.
///
/// Callers MUST pass observations sorted by ascending timestamp.
pub fn coalesce_incidents<I>(target_id: Uuid, observations: I) -> Vec<Incident>
where
    I: IntoIterator<Item = (DateTime<Utc>, CheckStatus, Option<String>)>,
{
    let mut out = Vec::new();
    let mut current: Option<Incident> = None;
    for (ts, status, error) in observations {
        let bad = matches!(status, CheckStatus::Down | CheckStatus::Error);
        if bad {
            if let Some(inc) = current.as_mut() {
                extend_incident(inc, ts, status, error);
            } else {
                current = Some(new_incident(target_id, ts, status, error));
            }
        } else if let Some(inc) = current.take() {
            out.push(inc);
        }
    }
    if let Some(mut inc) = current {
        // Trailing bad run is still ongoing — clear the `ended_at`/`duration_secs`
        // we may have populated from earlier observations in the same run.
        inc.ended_at = None;
        inc.duration_secs = None;
        out.push(inc);
    }
    out
}

fn extend_incident(
    inc: &mut Incident,
    ts: DateTime<Utc>,
    status: CheckStatus,
    error: Option<String>,
) {
    inc.check_count += 1;
    if inc.error_sample.is_none() {
        inc.error_sample = error;
    }
    inc.ended_at = Some(ts);
    inc.duration_secs = Some((ts - inc.started_at).num_seconds().max(0) as u64);
    inc.status = status;
}

fn new_incident(
    target_id: Uuid,
    ts: DateTime<Utc>,
    status: CheckStatus,
    error: Option<String>,
) -> Incident {
    Incident {
        id: Uuid::now_v7(),
        target_id: Some(target_id),
        started_at: ts,
        ended_at: None,
        status,
        duration_secs: None,
        check_count: 1,
        error_sample: error,
        severity: IncidentSeverity::default(),
        counts_as_downtime: true,
        public_title: None,
        public_description: None,
        created_at: None,
        updated_at: None,
        updates: Vec::new(),
        regions_down: Vec::new(),
        regions_up: Vec::new(),
    }
}

// ───────────────────────── Operational incident model ─────────────────────
// The narration `Incident` above is the PUBLIC projection — it is serialised
// to the public API and MCP. The types below are the INTERNAL operational
// projection over the same `incidents` row: state machine, ownership,
// escalation bookkeeping. They are a deliberately separate read-model so
// internal fields (assignee, acknowledged_by, next_escalation_at) can never
// leak into a public response.

/// Internal operational state. Orthogonal to the public `IncidentStatusPhase`:
/// acknowledging halts paging, it does not post a customer-facing update.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum IncidentState {
    #[default]
    Triggered,
    Acknowledged,
    Resolved,
}

impl IncidentState {
    pub const ALL: &'static [Self] = &[Self::Triggered, Self::Acknowledged, Self::Resolved];

    pub fn as_db_str(self) -> &'static str {
        match self {
            Self::Triggered => "triggered",
            Self::Acknowledged => "acknowledged",
            Self::Resolved => "resolved",
        }
    }

    pub fn from_db_str(s: &str) -> Self {
        match s {
            "acknowledged" => Self::Acknowledged,
            "resolved" => Self::Resolved,
            _ => Self::Triggered,
        }
    }

    /// `true` while the incident is still actionable (not yet resolved).
    pub fn is_open(self) -> bool {
        matches!(self, Self::Triggered | Self::Acknowledged)
    }
}

/// Paging behaviour. `high` pages on-call; `low` records the incident but does
/// not page (PagerDuty's urgency split, distinct from customer-facing severity).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum IncidentUrgency {
    #[default]
    High,
    Low,
}

impl IncidentUrgency {
    pub const ALL: &'static [Self] = &[Self::High, Self::Low];
    pub fn as_db_str(self) -> &'static str {
        match self {
            Self::High => "high",
            Self::Low => "low",
        }
    }
    pub fn from_db_str(s: &str) -> Self {
        match s {
            "low" => Self::Low,
            _ => Self::High,
        }
    }
}

/// How the incident came to exist: auto-opened from a monitor's check stream,
/// or manually declared by an operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum IncidentOrigin {
    #[default]
    Monitor,
    Manual,
}

impl IncidentOrigin {
    pub const ALL: &'static [Self] = &[Self::Monitor, Self::Manual];
    pub fn as_db_str(self) -> &'static str {
        match self {
            Self::Monitor => "monitor",
            Self::Manual => "manual",
        }
    }
    pub fn from_db_str(s: &str) -> Self {
        match s {
            "manual" => Self::Manual,
            _ => Self::Monitor,
        }
    }
}

/// Whether the incident is published to a public status page.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum IncidentVisibility {
    #[default]
    Internal,
    Public,
}

impl IncidentVisibility {
    pub const ALL: &'static [Self] = &[Self::Internal, Self::Public];
    pub fn as_db_str(self) -> &'static str {
        match self {
            Self::Internal => "internal",
            Self::Public => "public",
        }
    }
    pub fn from_db_str(s: &str) -> Self {
        match s {
            "public" => Self::Public,
            _ => Self::Internal,
        }
    }
}

/// Who caused an `IncidentEvent`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum ActorType {
    System,
    User,
    Mcp,
    /// A person acting through a signed link or a push app's own
    /// acknowledgement. Possession of the notification is the whole proof, so
    /// there is no user to name.
    Link,
}

impl ActorType {
    pub const ALL: &'static [Self] = &[Self::System, Self::User, Self::Mcp, Self::Link];
    pub fn as_db_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::User => "user",
            Self::Mcp => "mcp",
            Self::Link => "link",
        }
    }
    pub fn from_db_str(s: &str) -> Self {
        match s {
            "user" => Self::User,
            "mcp" => Self::Mcp,
            "link" => Self::Link,
            _ => Self::System,
        }
    }
}

/// Kind of internal timeline entry. Mirrors the `incident_events.kind` CHECK.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum IncidentEventKind {
    Triggered,
    Acknowledged,
    Assigned,
    Unassigned,
    Escalated,
    Notified,
    Note,
    SeverityChanged,
    DowntimeChanged,
    StateChanged,
    Resolved,
    Reopened,
    Published,
    Unpublished,
    PostmortemPublished,
    PostmortemUnpublished,
}

impl IncidentEventKind {
    pub const ALL: &'static [Self] = &[
        Self::Triggered,
        Self::Acknowledged,
        Self::Assigned,
        Self::Unassigned,
        Self::Escalated,
        Self::Notified,
        Self::Note,
        Self::SeverityChanged,
        Self::DowntimeChanged,
        Self::StateChanged,
        Self::Resolved,
        Self::Reopened,
        Self::Published,
        Self::Unpublished,
        Self::PostmortemPublished,
        Self::PostmortemUnpublished,
    ];
    pub fn as_db_str(self) -> &'static str {
        match self {
            Self::Triggered => "triggered",
            Self::Acknowledged => "acknowledged",
            Self::Assigned => "assigned",
            Self::Unassigned => "unassigned",
            Self::Escalated => "escalated",
            Self::Notified => "notified",
            Self::Note => "note",
            Self::SeverityChanged => "severity_changed",
            Self::DowntimeChanged => "downtime_changed",
            Self::StateChanged => "state_changed",
            Self::Resolved => "resolved",
            Self::Reopened => "reopened",
            Self::Published => "published",
            Self::Unpublished => "unpublished",
            Self::PostmortemPublished => "postmortem_published",
            Self::PostmortemUnpublished => "postmortem_unpublished",
        }
    }
    /// Unknown values fall back to `Note` so a forward-migrated row never
    /// panics a read path.
    pub fn from_db_str(s: &str) -> Self {
        match s {
            "triggered" => Self::Triggered,
            "acknowledged" => Self::Acknowledged,
            "assigned" => Self::Assigned,
            "unassigned" => Self::Unassigned,
            "escalated" => Self::Escalated,
            "notified" => Self::Notified,
            "severity_changed" => Self::SeverityChanged,
            "downtime_changed" => Self::DowntimeChanged,
            "state_changed" => Self::StateChanged,
            "resolved" => Self::Resolved,
            "reopened" => Self::Reopened,
            "published" => Self::Published,
            "unpublished" => Self::Unpublished,
            "postmortem_published" => Self::PostmortemPublished,
            "postmortem_unpublished" => Self::PostmortemUnpublished,
            _ => Self::Note,
        }
    }
}

/// Delivery status of a paging attempt (`incident_notifications.status`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum NotificationStatus {
    Queued,
    Sent,
    Failed,
    Suppressed,
}

impl NotificationStatus {
    pub const ALL: &'static [Self] = &[Self::Queued, Self::Sent, Self::Failed, Self::Suppressed];
    pub fn as_db_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Sent => "sent",
            Self::Failed => "failed",
            Self::Suppressed => "suppressed",
        }
    }
    pub fn from_db_str(s: &str) -> Self {
        match s {
            "sent" => Self::Sent,
            "failed" => Self::Failed,
            "suppressed" => Self::Suppressed,
            _ => Self::Queued,
        }
    }
}

/// Why a page was sent (`incident_notifications.reason`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum NotificationReason {
    Opened,
    Escalated,
    Resolved,
    Reopened,
    /// Monitoring stopped: the probes covering this monitor all went silent
    /// (no incident — orthogonal to up/down). Distinct wording: "no data".
    NoData,
    /// Monitoring resumed after a `NoData` notice.
    DataResumed,
    /// Periodic nudge that an already-paged incident is still unacknowledged.
    Reminder,
}

impl NotificationReason {
    pub const ALL: &'static [Self] = &[
        Self::Opened,
        Self::Escalated,
        Self::Resolved,
        Self::Reopened,
        Self::NoData,
        Self::DataResumed,
        Self::Reminder,
    ];
    pub fn as_db_str(self) -> &'static str {
        match self {
            Self::Opened => "opened",
            Self::Escalated => "escalated",
            Self::Resolved => "resolved",
            Self::Reopened => "reopened",
            Self::NoData => "no_data",
            Self::DataResumed => "data_resumed",
            Self::Reminder => "reminder",
        }
    }
    pub fn from_db_str(s: &str) -> Self {
        match s {
            "escalated" => Self::Escalated,
            "resolved" => Self::Resolved,
            "reopened" => Self::Reopened,
            "no_data" => Self::NoData,
            "data_resumed" => Self::DataResumed,
            "reminder" => Self::Reminder,
            _ => Self::Opened,
        }
    }
}

/// Internal operational projection of an `incidents` row. Returned by the
/// `IncidentOpsStore`; never serialised to a public/anonymous surface.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct OpsIncident {
    pub id: Uuid,
    #[schema(nullable = true)]
    pub target_id: Option<Uuid>,
    #[schema(nullable = true)]
    pub title: Option<String>,
    pub state: IncidentState,
    pub severity: IncidentSeverity,
    pub urgency: IncidentUrgency,
    pub origin: IncidentOrigin,
    pub visibility: IncidentVisibility,
    /// Whether this incident may page at all. Honoured by every open-side
    /// path, not just the first signal: the reconcile sweep pages anything
    /// triggered that reached no channel.
    #[serde(default = "default_true")]
    pub paging_enabled: bool,
    #[serde(default = "default_true")]
    pub counts_as_downtime: bool,
    pub started_at: DateTime<Utc>,
    #[schema(nullable = true)]
    pub ended_at: Option<DateTime<Utc>>,
    #[schema(nullable = true)]
    pub acknowledged_at: Option<DateTime<Utc>>,
    #[schema(nullable = true)]
    pub acknowledged_by: Option<UserId>,
    #[schema(nullable = true)]
    pub assigned_to: Option<UserId>,
    #[schema(nullable = true)]
    pub resolved_by: Option<UserId>,
    #[schema(nullable = true)]
    pub escalation_policy_id: Option<Uuid>,
    pub escalation_level: i32,
    pub escalation_round: i32,
    #[schema(nullable = true)]
    pub next_escalation_at: Option<DateTime<Utc>>,
    pub check_count: u64,
    #[schema(nullable = true)]
    pub error_sample: Option<String>,
    /// Regions that have confirmed the failure at any point in this incident,
    /// and those that have not (empty = single-region).
    #[serde(default)]
    pub regions_down: Vec<String>,
    #[serde(default)]
    pub regions_up: Vec<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

fn default_true() -> bool {
    true
}

/// Operator-declared incident not driven by a monitor's check stream.
#[derive(Debug, Clone, Default, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct NewManualIncident {
    #[serde(default)]
    #[schema(nullable = true)]
    pub title: Option<String>,
    #[serde(default)]
    pub severity: IncidentSeverity,
    #[serde(default)]
    pub urgency: IncidentUrgency,
    /// Optional monitor to associate; manual incidents may stand alone.
    #[serde(default)]
    #[schema(nullable = true)]
    pub target_id: Option<Uuid>,
    /// Publish to the status pages carrying the monitor as the incident opens.
    #[serde(default)]
    pub visibility: IncidentVisibility,
    /// Alert the org's channels. Off by default: an unasked-for page reads as
    /// the product misfiring.
    #[serde(default)]
    pub notify: bool,
    /// Off by default: a declaration has no failing check behind it.
    #[serde(default)]
    pub counts_as_downtime: bool,
}

/// One internal timeline entry.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct IncidentEvent {
    pub id: Uuid,
    pub incident_id: Uuid,
    pub occurred_at: DateTime<Utc>,
    pub kind: IncidentEventKind,
    pub actor_type: ActorType,
    #[schema(nullable = true)]
    pub actor_id: Option<UserId>,
    pub detail: Value,
    #[schema(nullable = true)]
    pub message: Option<String>,
}

/// One paging delivery-log entry.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct IncidentNotification {
    pub id: Uuid,
    pub incident_id: Uuid,
    #[schema(nullable = true)]
    pub escalation_level: Option<i32>,
    #[schema(nullable = true)]
    pub target_user_id: Option<UserId>,
    #[schema(nullable = true)]
    pub channel_id: Option<Uuid>,
    pub transport: String,
    pub reason: NotificationReason,
    pub status: NotificationStatus,
    pub attempt: i32,
    #[schema(nullable = true)]
    pub error: Option<String>,
    pub created_at: DateTime<Utc>,
    #[schema(nullable = true)]
    pub sent_at: Option<DateTime<Utc>>,
    /// When the retry sweep will next re-attempt a failed delivery; `None` once
    /// it has been sent, suppressed, or dead-lettered (attempts exhausted).
    #[schema(nullable = true)]
    pub next_attempt_at: Option<DateTime<Utc>>,
    /// Pushover emergency receipt id, polled for acknowledgement and cancelled
    /// when the incident resolves. `None` for ordinary deliveries.
    #[schema(nullable = true)]
    pub provider_receipt: Option<String>,
    /// When the recipient acknowledged an emergency page (Pushover). `None`
    /// until acknowledged, or for non-emergency deliveries.
    #[schema(nullable = true)]
    pub acked_at: Option<DateTime<Utc>>,
}

/// A paging delivery-log row to persist. The store stamps `id`/`created_at`.
#[derive(Debug, Clone)]
pub struct NewIncidentNotification {
    pub org: OrgId,
    pub incident_id: Uuid,
    pub escalation_level: Option<i32>,
    pub target_user_id: Option<UserId>,
    pub channel_id: Option<Uuid>,
    pub transport: String,
    pub reason: NotificationReason,
    pub status: NotificationStatus,
    pub attempt: i32,
    pub error: Option<String>,
    pub sent_at: Option<DateTime<Utc>>,
}

/// Result of one paging attempt, applied to its `incident_notifications` row by
/// `IncidentOpsStore::mark_notification`.
#[derive(Debug, Clone)]
pub struct NotificationOutcome {
    pub status: NotificationStatus,
    pub attempt: i32,
    pub error: Option<String>,
    pub sent_at: Option<DateTime<Utc>>,
    /// Schedules the next retry (backoff), or clears it once the row is sent,
    /// suppressed, or dead-lettered (attempts exhausted).
    pub next_attempt_at: Option<DateTime<Utc>>,
    /// Pushover emergency receipt id captured on a successful send; persisted so
    /// the poll/cancel steps can find it. `None` for ordinary deliveries.
    pub provider_receipt: Option<String>,
}

// ───────────────────────── Lifecycle state machine ────────────────────────

/// A requested operational transition. `Resolve` is a human action;
/// `AutoResolve` is the writer detecting recovery — they reach the same state
/// but the store records a different `resolved_by` (a user vs `NULL`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IncidentTransition {
    Acknowledge,
    Resolve,
    AutoResolve,
    Reopen,
}

/// An illegal transition for the incident's current state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransitionError {
    pub from: IncidentState,
    pub transition: IncidentTransition,
}

impl std::fmt::Display for TransitionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "illegal incident transition {:?} from state {}",
            self.transition,
            self.from.as_db_str()
        )
    }
}

impl std::error::Error for TransitionError {}

/// Pure state-machine step. Returns the resulting state or rejects an illegal
/// transition. Acknowledge and resolve are
/// idempotent within an open/resolved state; reopen is only valid from
/// `resolved`; a resolved incident cannot be acknowledged (reopen first).
pub fn next_state(
    current: IncidentState,
    transition: IncidentTransition,
) -> Result<IncidentState, TransitionError> {
    use IncidentState::*;
    use IncidentTransition::*;
    let err = || TransitionError {
        from: current,
        transition,
    };
    match (current, transition) {
        // Acknowledge: only an open incident; idempotent while acknowledged.
        (Triggered | Acknowledged, Acknowledge) => Ok(Acknowledged),
        (Resolved, Acknowledge) => Err(err()),
        // Resolve (manual or auto): from any open state; idempotent if resolved.
        (Triggered | Acknowledged | Resolved, Resolve | AutoResolve) => Ok(Resolved),
        // Reopen: only a resolved incident returns to triggered.
        (Resolved, Reopen) => Ok(Triggered),
        (Triggered | Acknowledged, Reopen) => Err(err()),
    }
}

/// One follow-up task captured in a postmortem.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ActionItem {
    pub text: String,
    #[serde(default)]
    #[schema(nullable = true)]
    pub owner_user_id: Option<UserId>,
    #[serde(default)]
    pub done: bool,
}

/// Request-side shape. The stored `ActionItem` stays lenient so a key added
/// later never breaks a read of what is already in the row.
#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ActionItemInput {
    text: String,
    #[serde(default)]
    #[schema(nullable = true)]
    owner_user_id: Option<UserId>,
    #[serde(default)]
    done: bool,
}

impl ActionItem {
    pub(crate) fn deserialize_strict_list<'de, D: serde::Deserializer<'de>>(
        d: D,
    ) -> Result<Vec<Self>, D::Error> {
        Vec::<ActionItemInput>::deserialize(d).map(|items| {
            items
                .into_iter()
                .map(|i| Self {
                    text: i.text,
                    owner_user_id: i.owner_user_id,
                    done: i.done,
                })
                .collect()
        })
    }
}

/// Retrospective document attached to a single incident.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct IncidentPostmortem {
    pub incident_id: Uuid,
    #[schema(nullable = true)]
    pub summary: Option<String>,
    #[schema(nullable = true)]
    pub root_cause: Option<String>,
    #[schema(nullable = true)]
    pub impact: Option<String>,
    pub action_items: Vec<ActionItem>,
    #[schema(nullable = true)]
    pub author_id: Option<UserId>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// `null` until the operator publishes the postmortem.
    #[schema(nullable = true)]
    pub published_at: Option<DateTime<Utc>>,
}

/// Operator-supplied postmortem content. Each field replaces the stored value;
/// `action_items` is the full list every save.
#[derive(Debug, Clone, Default, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PostmortemUpsert {
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub root_cause: Option<String>,
    #[serde(default)]
    pub impact: Option<String>,
    #[serde(default, deserialize_with = "ActionItem::deserialize_strict_list")]
    #[schema(value_type = Vec<ActionItemInput>)]
    pub action_items: Vec<ActionItem>,
}

/// One severity/state bucket in the incident metrics rollup.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct MetricBucket {
    pub key: String,
    pub count: u64,
}

/// Noisiest monitor in the window, by incident count.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct MonitorIncidentCount {
    pub target_id: Uuid,
    pub count: u64,
}

/// Aggregate incident reporting over a trailing window. `mtta`/`mttr` are means
/// in seconds, `null` when no incident in the window qualifies (none acked, or
/// none resolved).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct IncidentMetrics {
    pub window_days: u32,
    pub total: u64,
    #[schema(nullable = true)]
    pub mtta_secs: Option<f64>,
    #[schema(nullable = true)]
    pub mttr_secs: Option<f64>,
    pub by_severity: Vec<MetricBucket>,
    pub by_state: Vec<MetricBucket>,
    pub auto_resolved: u64,
    pub human_resolved: u64,
    pub top_monitors: Vec<MonitorIncidentCount>,
}

#[cfg(test)]
mod transition_tests {
    use super::*;

    #[test]
    fn acknowledge_opens_then_idempotent() {
        assert_eq!(
            next_state(IncidentState::Triggered, IncidentTransition::Acknowledge),
            Ok(IncidentState::Acknowledged)
        );
        assert_eq!(
            next_state(IncidentState::Acknowledged, IncidentTransition::Acknowledge),
            Ok(IncidentState::Acknowledged)
        );
    }

    #[test]
    fn cannot_acknowledge_resolved() {
        assert!(next_state(IncidentState::Resolved, IncidentTransition::Acknowledge).is_err());
    }

    #[test]
    fn resolve_from_any_open_state_and_idempotent() {
        for from in [
            IncidentState::Triggered,
            IncidentState::Acknowledged,
            IncidentState::Resolved,
        ] {
            assert_eq!(
                next_state(from, IncidentTransition::Resolve),
                Ok(IncidentState::Resolved)
            );
            assert_eq!(
                next_state(from, IncidentTransition::AutoResolve),
                Ok(IncidentState::Resolved)
            );
        }
    }

    #[test]
    fn reopen_only_from_resolved() {
        assert_eq!(
            next_state(IncidentState::Resolved, IncidentTransition::Reopen),
            Ok(IncidentState::Triggered)
        );
        assert!(next_state(IncidentState::Triggered, IncidentTransition::Reopen).is_err());
        assert!(next_state(IncidentState::Acknowledged, IncidentTransition::Reopen).is_err());
    }

    #[test]
    fn is_open_matches_non_resolved() {
        assert!(IncidentState::Triggered.is_open());
        assert!(IncidentState::Acknowledged.is_open());
        assert!(!IncidentState::Resolved.is_open());
    }

    #[test]
    fn db_str_roundtrips() {
        for s in IncidentState::ALL {
            assert_eq!(IncidentState::from_db_str(s.as_db_str()), *s);
        }
        for u in IncidentUrgency::ALL {
            assert_eq!(IncidentUrgency::from_db_str(u.as_db_str()), *u);
        }
        for o in IncidentOrigin::ALL {
            assert_eq!(IncidentOrigin::from_db_str(o.as_db_str()), *o);
        }
        for v in IncidentVisibility::ALL {
            assert_eq!(IncidentVisibility::from_db_str(v.as_db_str()), *v);
        }
        for a in ActorType::ALL {
            assert_eq!(ActorType::from_db_str(a.as_db_str()), *a);
        }
        for k in IncidentEventKind::ALL {
            assert_eq!(IncidentEventKind::from_db_str(k.as_db_str()), *k);
        }
        for s in NotificationStatus::ALL {
            assert_eq!(NotificationStatus::from_db_str(s.as_db_str()), *s);
        }
        for r in NotificationReason::ALL {
            assert_eq!(NotificationReason::from_db_str(r.as_db_str()), *r);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn ts(seconds_from_epoch: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(seconds_from_epoch, 0).single().unwrap()
    }

    #[test]
    fn confirmed_downtime_clamps_to_window_and_now() {
        let (from, to, now) = (ts(1000), ts(2000), ts(2000));
        let mk = |start: i64, end: Option<i64>| {
            let mut i = new_incident(Uuid::nil(), ts(start), CheckStatus::Down, None);
            i.ended_at = end.map(ts);
            i
        };
        let incidents = vec![
            mk(1200, Some(1300)), // inside: 100s
            mk(900, Some(1100)),  // straddles start -> 100s
            mk(1800, None),       // ongoing -> clamp to now: 200s
            mk(100, Some(200)),   // before window: 0
        ];
        assert_eq!(confirmed_downtime_secs(&incidents, from, to, now), 400);
        assert!((uptime_pct_from_downtime(400, 1000) - 60.0).abs() < 1e-9);
    }

    /// An excluded row is listed but must not reach the sum.
    #[test]
    fn an_excluded_incident_contributes_no_downtime() {
        let (from, to, now) = (ts(1000), ts(2000), ts(2000));
        let mut counted = new_incident(Uuid::nil(), ts(1200), CheckStatus::Down, None);
        counted.ended_at = Some(ts(1300));
        let mut excluded = new_incident(Uuid::nil(), ts(1400), CheckStatus::Down, None);
        excluded.ended_at = Some(ts(1900));
        excluded.counts_as_downtime = false;
        let incidents = vec![counted, excluded];
        assert_eq!(confirmed_downtime_secs(&incidents, from, to, now), 100);
    }

    /// A client that omits these cannot page an org by accident.
    #[test]
    fn a_declare_body_that_says_nothing_pages_nobody_and_stays_internal() {
        let new: NewManualIncident = serde_json::from_str(r#"{"title":"db failover"}"#).unwrap();
        assert!(!new.notify);
        assert_eq!(new.visibility, IncidentVisibility::Internal);
        assert!(!new.counts_as_downtime);
    }

    /// Rows predating the column are measured downtime, so the default is on.
    #[test]
    fn an_incident_json_without_the_flag_still_counts() {
        let inc: Incident = serde_json::from_str(
            r#"{"id":"00000000-0000-0000-0000-000000000000","target_id":null,
                "started_at":"2026-09-01T13:24:22Z","ended_at":null,"status":"down",
                "duration_secs":null,"check_count":2,"error_sample":null}"#,
        )
        .unwrap();
        assert!(inc.counts_as_downtime);
    }

    #[test]
    fn a_declare_body_can_ask_for_both() {
        let new: NewManualIncident =
            serde_json::from_str(r#"{"title":"x","notify":true,"visibility":"public"}"#).unwrap();
        assert!(new.notify);
        assert_eq!(new.visibility, IncidentVisibility::Public);
    }

    #[test]
    fn uptime_pct_edge_cases() {
        assert_eq!(uptime_pct_from_downtime(0, 1000), 100.0);
        assert_eq!(uptime_pct_from_downtime(1000, 1000), 0.0);
        assert_eq!(uptime_pct_from_downtime(5000, 1000), 0.0); // over-window clamps
        assert_eq!(uptime_pct_from_downtime(0, 0), 100.0); // no window
    }
}
