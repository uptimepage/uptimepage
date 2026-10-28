use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use super::{CheckStatus, MaintenanceUpdate, MaintenanceUpdatePhase};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum OverallState {
    Operational,
    Maintenance,
    MinorDisruption,
    PartialOutage,
    MajorOutage,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct OverallStatus {
    pub state: OverallState,
    #[schema(example = "All Systems Operational")]
    pub label: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum PublicComponentStatus {
    Operational,
    Degraded,
    PartialOutage,
    MajorOutage,
    Maintenance,
    /// Nothing recorded anywhere in the history window. A heartbeat that has
    /// never been pinged sits here; calling that operational is the worse lie.
    NoData,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum DayState {
    Operational,
    Degraded,
    PartialOutage,
    MajorOutage,
    Maintenance,
    NoData,
}

/// When an incident ran, and how bad it was.
pub type ImpactSpan = (DateTime<Utc>, DateTime<Utc>, IncidentImpact);

/// A stretch inside an incident when the monitor was back up, until a failure
/// that confirmed inside its recovery period took it down again. It belongs to
/// the incident but is not downtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Recovered {
    pub from: DateTime<Utc>,
    pub until: DateTime<Utc>,
}

impl Recovered {
    /// The stretches a stored incident keeps as paired arrays.
    pub fn paired(from: Vec<DateTime<Utc>>, until: Vec<DateTime<Utc>>) -> Vec<Self> {
        from.into_iter()
            .zip(until)
            .map(|(from, until)| Self { from, until })
            .collect()
    }
}

/// When a monitor-opened incident's monitor was paused. Nobody checked it then,
/// so none of it is downtime, though the incident stays open across a pause.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Paused {
    resumed: Vec<(DateTime<Utc>, DateTime<Utc>)>,
    since: Option<DateTime<Utc>>,
}

impl Paused {
    /// As a stored incident keeps it: the pauses it was resumed from as paired
    /// arrays, and the start of the one still running.
    pub fn stored(
        from: Vec<DateTime<Utc>>,
        until: Vec<DateTime<Utc>>,
        since: Option<DateTime<Utc>>,
    ) -> Self {
        Self {
            resumed: from.into_iter().zip(until).collect(),
            since,
        }
    }

    /// Each pause inside an incident that ended at `ended`, until its resume or
    /// that end; `None` for one still running on an open incident.
    pub fn spans(
        &self,
        ended: Option<DateTime<Utc>>,
    ) -> impl Iterator<Item = (DateTime<Utc>, Option<DateTime<Utc>>)> + '_ {
        self.resumed
            .iter()
            .map(|&(from, until)| (from, Some(until)))
            .chain(self.since.map(|s| (s, None)))
            .filter(move |(from, _)| ended.is_none_or(|e| *from < e))
            .map(move |(from, until)| match (until, ended) {
                (Some(u), Some(e)) => (from, Some(u.min(e))),
                (until, ended) => (from, until.or(ended)),
            })
    }

    fn stretches(&self) -> impl Iterator<Item = (DateTime<Utc>, DateTime<Utc>)> + '_ {
        self.spans(None)
            .map(|(from, until)| (from, until.unwrap_or(DateTime::<Utc>::MAX_UTC)))
    }
}

/// The parts of an incident the service was down: from `started` to `ended`
/// (`None` while open, and so is the last part unless a pause is running) less
/// the stretches it was back up in or paused in.
pub fn outage_parts(
    started: DateTime<Utc>,
    ended: Option<DateTime<Utc>>,
    recovered: &[Recovered],
    paused: &Paused,
) -> Vec<(DateTime<Utc>, Option<DateTime<Utc>>)> {
    let end = ended.unwrap_or(DateTime::<Utc>::MAX_UTC);
    let mut stretches: Vec<(DateTime<Utc>, DateTime<Utc>)> = recovered
        .iter()
        .map(|r| (r.from, r.until))
        .chain(paused.stretches())
        .filter(|(from, until)| until > from)
        .collect();
    stretches.sort_unstable();
    let mut parts = Vec::with_capacity(stretches.len() + 1);
    let mut from = started;
    for (gap_from, gap_until) in stretches {
        if gap_from > from {
            parts.push((from, Some(gap_from.min(end))));
        }
        from = from.max(gap_until);
        if from >= end {
            return parts;
        }
    }
    parts.push((from, ended));
    parts
}

/// Time one monitor or component spent in each outage state over a range. An
/// instant covered by two incidents counts once, at the worse of them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Downtime {
    pub major_secs: i64,
    pub partial_secs: i64,
    pub degraded_secs: i64,
}

impl Downtime {
    /// Seconds in each state over `[from, to)`. Spans are clipped to the
    /// range; an open incident is passed ending at `now`.
    pub fn between(spans: &[ImpactSpan], from: DateTime<Utc>, to: DateTime<Utc>) -> Self {
        let mut out = Self::default();
        if to <= from {
            return out;
        }
        let mut edges: Vec<(DateTime<Utc>, IncidentImpact, i32)> = Vec::new();
        for &(start, end, impact) in spans {
            let (start, end) = (start.clamp(from, to), end.clamp(from, to));
            if start < end {
                edges.push((start, impact, 1));
                edges.push((end, impact, -1));
            }
        }
        edges.sort_unstable_by_key(|(at, ..)| *at);
        let mut open = [0i32; 3];
        let mut since = from;
        for (at, impact, delta) in edges {
            let secs = (at - since).num_seconds();
            if open[2] > 0 {
                out.major_secs += secs;
            } else if open[1] > 0 {
                out.partial_secs += secs;
            } else if open[0] > 0 {
                out.degraded_secs += secs;
            }
            open[impact.rank()] += delta;
            since = at;
        }
        out
    }

    /// Downtime as uptime figures and the day strip weigh it: a partial outage
    /// counts for 30% of its length, as on Atlassian Statuspage, and degraded
    /// performance is not downtime at all.
    pub fn weighted_secs(&self) -> i64 {
        self.major_secs + self.partial_secs * 3 / 10
    }

    /// Time something was out, whole or in part; degraded performance is not.
    pub fn outage_secs(&self) -> i64 {
        self.major_secs + self.partial_secs
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct PublicComponent {
    pub id: Uuid,
    pub name: String,
    #[schema(nullable = true)]
    pub description: Option<String>,
    pub current_status: PublicComponentStatus,
    /// Daily history, oldest first.
    pub history: Vec<DayState>,
    /// Time in each outage state per day, aligned with `history`. Tints the
    /// strip; not part of the wire shape.
    #[serde(skip)]
    pub downtime: Vec<Downtime>,
    /// Whether a check ran each day, aligned with `history`. A maintenance
    /// day counts as a day with data only when one did; not part of the
    /// wire shape.
    #[serde(skip)]
    pub checked: Vec<bool>,
    /// Confirmed-incident downtime over the time the component was probed
    /// within the history span, as a percentage; `null` until it is probed.
    #[serde(default)]
    #[schema(nullable = true)]
    pub uptime_pct: Option<f64>,
    /// Path of the component's read-only detail view on the page's own host,
    /// when the page links one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = true, example = "/m/3q2xW9")]
    pub detail_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct PublicComponentGroup {
    #[schema(nullable = true, example = "API")]
    pub name: Option<String>,
    pub components: Vec<PublicComponent>,
    /// A monitor group's section only: its members' combined state, by the
    /// rule the page's own banner uses. Absent on a hand-made heading.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = true)]
    pub status: Option<OverallState>,
}

/// What an incident did to its component, in the page's own words: measured
/// from the probes for a monitor-opened incident, taken from the declared
/// severity for a manual one. The day strip, the incident cards and the API
/// all speak this one vocabulary. `Ord` ranks by impact so "worst wins" is
/// `Iterator::max`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum IncidentImpact {
    Degraded,
    PartialOutage,
    MajorOutage,
}

impl IncidentImpact {
    /// The wire name, as it serialises.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Degraded => "degraded",
            Self::PartialOutage => "partial_outage",
            Self::MajorOutage => "major_outage",
        }
    }

    fn rank(self) -> usize {
        match self {
            Self::Degraded => 0,
            Self::PartialOutage => 1,
            Self::MajorOutage => 2,
        }
    }
}

/// Impact of one confirmed incident. `degraded` is whether the incident
/// opened on a `degraded` check status (slow / rate-limited, not hard-failed);
/// `any_region_up` is whether some region still answered when it opened —
/// present regions in `regions_up` mean a partial, not total, loss.
pub fn incident_impact(degraded: bool, any_region_up: bool) -> IncidentImpact {
    if degraded {
        IncidentImpact::Degraded
    } else if any_region_up {
        IncidentImpact::PartialOutage
    } else {
        IncidentImpact::MajorOutage
    }
}

/// [`incident_impact`] read off a stored incident row. Manual incidents carry
/// the operator's chosen severity; the check fields are placeholders on them.
/// Monitor-opened incidents derive from what the probes saw, so the same
/// outage reads the same on the day strip, the incident card, the uptime
/// figures and the API.
pub fn stored_incident_impact(
    origin: &str,
    severity: IncidentSeverity,
    status_at_start: &str,
    regions_up: Option<&[String]>,
) -> IncidentImpact {
    if origin == "manual" {
        return match severity {
            IncidentSeverity::Minor => IncidentImpact::Degraded,
            IncidentSeverity::Major => IncidentImpact::PartialOutage,
            IncidentSeverity::Critical => IncidentImpact::MajorOutage,
        };
    }
    incident_impact(
        status_at_start == CheckStatus::Degraded.as_str(),
        regions_up.is_some_and(|r| !r.is_empty()),
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum IncidentSeverity {
    Minor,
    #[default]
    Major,
    Critical,
}

impl IncidentSeverity {
    /// Every variant in declaration order. Used by the enum-drift integration
    /// test to compare against the live Postgres CHECK constraint; keep in
    /// lockstep with the enum body. (Adding a variant without extending
    /// `ALL` lets the drift test pass while the migration list silently
    /// disagrees, so this list is itself a load-bearing invariant.)
    pub const ALL: &'static [Self] = &[Self::Minor, Self::Major, Self::Critical];

    /// Stable string used in the Postgres `severity` CHECK constraint and the
    /// JSON wire form. Unknown DB values fall back to `Major` (defensive
    /// against migrations / corruption — never panics on parse).
    pub fn as_db_str(self) -> &'static str {
        match self {
            Self::Minor => "minor",
            Self::Major => "major",
            Self::Critical => "critical",
        }
    }

    pub fn from_db_str(s: &str) -> Self {
        match s {
            "minor" => Self::Minor,
            "critical" => Self::Critical,
            _ => Self::Major,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum IncidentStatusPhase {
    Investigating,
    Identified,
    Monitoring,
    Resolved,
    Postmortem,
}

impl IncidentStatusPhase {
    /// Every variant in declaration order; see the matching comment on
    /// `IncidentSeverity::ALL`.
    pub const ALL: &'static [Self] = &[
        Self::Investigating,
        Self::Identified,
        Self::Monitoring,
        Self::Resolved,
        Self::Postmortem,
    ];

    /// The phase a visitor sees: the latest update's, or Monitoring while the
    /// monitor is back up and waiting out its recovery. Derived, so the saved
    /// phase returns if the failure does, and an operator's own Monitoring,
    /// Resolved or Postmortem stands.
    pub fn shown(self, recovering: bool) -> Self {
        match self {
            Self::Investigating | Self::Identified if recovering => Self::Monitoring,
            phase => phase,
        }
    }

    /// Stable string used in the Postgres `phase` CHECK constraint and the
    /// JSON wire form. Unknown DB values fall back to `Investigating` so a
    /// migration / corruption never panics a read path.
    pub fn as_db_str(self) -> &'static str {
        match self {
            Self::Investigating => "investigating",
            Self::Identified => "identified",
            Self::Monitoring => "monitoring",
            Self::Resolved => "resolved",
            Self::Postmortem => "postmortem",
        }
    }

    pub fn from_db_str(s: &str) -> Self {
        match s {
            "identified" => Self::Identified,
            "monitoring" => Self::Monitoring,
            "resolved" => Self::Resolved,
            "postmortem" => Self::Postmortem,
            _ => Self::Investigating,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct PublicIncidentUpdate {
    pub posted_at: DateTime<Utc>,
    pub phase: IncidentStatusPhase,
    pub message: String,
    /// The platform wrote the words (a detection or recovery note, or the
    /// default line of a resolve or publish without one), so a listing can
    /// leave the boilerplate out. Not part of the wire shape.
    #[serde(skip)]
    pub generated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct PublicIncident {
    pub id: Uuid,
    /// `null` for an incident posted to the page itself rather than raised by
    /// one of its components; `component_name` is then empty.
    #[schema(nullable = true)]
    pub component_id: Option<Uuid>,
    pub component_name: String,
    /// `public_title` if set; otherwise auto-generated like `"API major outage"`.
    pub title: String,
    pub started_at: DateTime<Utc>,
    #[schema(nullable = true)]
    pub ended_at: Option<DateTime<Utc>>,
    /// The operator's declared severity. On a monitor-opened incident this is
    /// the default unless narrated; `impact` is what the page shows.
    pub severity: IncidentSeverity,
    pub impact: IncidentImpact,
    /// The latest update's phase, `investigating` if none, or `monitoring`
    /// while the monitor is back up and waiting out its recovery period. An
    /// update's own `monitoring`, `resolved` or `postmortem` stands.
    pub status_phase: IncidentStatusPhase,
    pub updates: Vec<PublicIncidentUpdate>,
    /// Stretches it was back up in before the failure returned; not downtime.
    /// Kept off the wire.
    #[serde(skip)]
    pub recovered: Vec<Recovered>,
    /// When its monitor was paused; not downtime. Kept off the wire.
    #[serde(skip)]
    pub paused: Paused,
    /// Back up and waiting out its recovery period; `status_phase` already
    /// says Monitoring. Kept off the wire.
    #[serde(skip)]
    pub recovering: bool,
    /// Present only once an operator publishes a postmortem; never set on list
    /// views.
    #[serde(default)]
    #[schema(nullable = true)]
    pub postmortem: Option<PublicPostmortem>,
}

/// One published postmortem action item. The internal owner is deliberately
/// omitted — only the public-safe task text and its done state are exposed.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct PublicActionItem {
    pub text: String,
    pub done: bool,
}

/// Customer-facing postmortem. Only surfaces after an operator publishes it.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct PublicPostmortem {
    #[schema(nullable = true)]
    pub summary: Option<String>,
    #[schema(nullable = true)]
    pub root_cause: Option<String>,
    #[schema(nullable = true)]
    pub impact: Option<String>,
    pub action_items: Vec<PublicActionItem>,
    pub published_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct PublicMaintenance {
    pub id: Uuid,
    pub title: String,
    #[schema(nullable = true)]
    pub description: Option<String>,
    pub starts_at: DateTime<Utc>,
    pub ends_at: DateTime<Utc>,
    pub affected_component_names: Vec<String>,
    /// What was posted about the window, oldest first.
    #[serde(default)]
    pub updates: Vec<PublicMaintenanceUpdate>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct PublicMaintenanceUpdate {
    pub posted_at: DateTime<Utc>,
    pub phase: MaintenanceUpdatePhase,
    pub message: String,
}

/// The public view leaves out who posted it.
impl From<MaintenanceUpdate> for PublicMaintenanceUpdate {
    fn from(u: MaintenanceUpdate) -> Self {
        Self {
            posted_at: u.posted_at,
            phase: u.phase,
            message: u.message,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct PublicMaintenanceList {
    pub active: Vec<PublicMaintenance>,
    pub upcoming: Vec<PublicMaintenance>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct PublicStatusPage {
    pub overall: OverallStatus,
    pub generated_at: DateTime<Utc>,
    /// The page's public name: its display name, else the org's name.
    pub site_name: String,
    pub groups: Vec<PublicComponentGroup>,
    pub active_incidents: Vec<PublicIncident>,
    /// Incidents that started in the last 30 days, newest first.
    pub recent_incidents: Vec<PublicIncident>,
    /// True when more incidents fall in that span than were listed.
    pub recent_incidents_has_more: bool,
    pub active_maintenance: Vec<PublicMaintenance>,
    pub upcoming_maintenance: Vec<PublicMaintenance>,
    /// Windows that ended, among those that started in the same 30 days,
    /// newest first. Cancelled windows are never listed.
    pub recent_maintenance: Vec<PublicMaintenance>,
    /// True when more ended windows fall in that span than were listed.
    pub recent_maintenance_has_more: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ComponentHistoryResponse {
    pub component_id: Uuid,
    pub component_name: String,
    pub days: u32,
    pub history: Vec<DayState>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_recovering_incident_reads_as_monitoring_until_an_operator_says_otherwise() {
        use IncidentStatusPhase::*;
        assert_eq!(Investigating.shown(true), Monitoring);
        assert_eq!(Identified.shown(true), Monitoring);
        assert_eq!(
            Investigating.shown(false),
            Investigating,
            "the saved phase returns"
        );
        for phase in [Monitoring, Resolved, Postmortem] {
            assert_eq!(phase.shown(true), phase);
        }
    }

    #[test]
    fn impact_degraded_wins_over_region_split() {
        assert_eq!(incident_impact(true, true), IncidentImpact::Degraded);
        assert_eq!(incident_impact(true, false), IncidentImpact::Degraded);
    }

    #[test]
    fn impact_some_region_up_is_partial() {
        assert_eq!(incident_impact(false, true), IncidentImpact::PartialOutage);
    }

    #[test]
    fn impact_no_region_up_is_major() {
        assert_eq!(incident_impact(false, false), IncidentImpact::MajorOutage);
    }

    #[test]
    fn impact_ord_ranks_major_worst() {
        assert!(IncidentImpact::MajorOutage > IncidentImpact::PartialOutage);
        assert!(IncidentImpact::PartialOutage > IncidentImpact::Degraded);
    }

    fn at(min: i64) -> DateTime<Utc> {
        use chrono::TimeZone;
        Utc.with_ymd_and_hms(2026, 10, 4, 0, 0, 0).unwrap() + chrono::Duration::minutes(min)
    }

    fn mins(secs: i64) -> i64 {
        secs / 60
    }

    #[test]
    fn an_incident_is_down_except_where_it_had_recovered() {
        let recovered = [
            Recovered {
                from: at(70),
                until: at(80),
            },
            Recovered {
                from: at(20),
                until: at(30),
            },
        ];
        assert_eq!(
            outage_parts(at(0), Some(at(100)), &recovered, &Paused::default()),
            [
                (at(0), Some(at(20))),
                (at(30), Some(at(70))),
                (at(80), Some(at(100)))
            ]
        );
        assert_eq!(
            outage_parts(at(0), None, &recovered, &Paused::default()).last(),
            Some(&(at(80), None)),
            "still down"
        );
        assert_eq!(
            outage_parts(at(0), Some(at(25)), &recovered, &Paused::default()),
            [(at(0), Some(at(20)))],
            "ended while recovered"
        );
    }

    #[test]
    fn a_paused_stretch_is_not_downtime() {
        let resumed = Paused::stored(vec![at(20)], vec![at(30)], None);
        assert_eq!(
            outage_parts(at(0), Some(at(100)), &[], &resumed),
            [(at(0), Some(at(20))), (at(30), Some(at(100)))]
        );
        let running = Paused::stored(vec![at(20)], vec![at(30)], Some(at(60)));
        assert_eq!(
            outage_parts(at(0), None, &[], &running),
            [(at(0), Some(at(20))), (at(30), Some(at(60)))],
            "an open incident stops at the pause still running"
        );
        assert_eq!(
            outage_parts(at(0), Some(at(90)), &[], &running).last(),
            Some(&(at(30), Some(at(60)))),
            "resolved while paused"
        );
        let recovered = [Recovered {
            from: at(10),
            until: at(25),
        }];
        assert_eq!(
            outage_parts(at(0), None, &recovered, &resumed),
            [(at(0), Some(at(10))), (at(30), None)],
            "overlapping stretches leave out their union"
        );
    }

    #[test]
    fn a_pause_is_shown_only_inside_its_incident() {
        let paused = Paused::stored(vec![at(10), at(50)], vec![at(30), at(70)], Some(at(90)));
        assert_eq!(
            paused.spans(Some(at(60))).collect::<Vec<_>>(),
            [(at(10), Some(at(30))), (at(50), Some(at(60)))],
            "clipped at the close, and none after it"
        );
        assert_eq!(
            paused.spans(None).last(),
            Some((at(90), None)),
            "still running on an open incident"
        );
    }

    #[test]
    fn overlapping_incidents_count_once_at_the_worse_state() {
        let spans = [
            (at(60), at(120), IncidentImpact::PartialOutage),
            (at(90), at(100), IncidentImpact::MajorOutage),
            (at(110), at(130), IncidentImpact::Degraded),
        ];
        let d = Downtime::between(&spans, at(0), at(24 * 60));
        assert_eq!(mins(d.major_secs), 10);
        assert_eq!(mins(d.partial_secs), 50, "60–90 and 100–120");
        assert_eq!(mins(d.degraded_secs), 10, "only 120–130 is degraded alone");
        assert_eq!(mins(d.outage_secs()), 60);
    }

    #[test]
    fn downtime_is_clipped_to_the_range() {
        let spans = [(at(-30), at(15), IncidentImpact::MajorOutage)];
        let d = Downtime::between(&spans, at(0), at(24 * 60));
        assert_eq!(mins(d.major_secs), 15);
        assert_eq!(
            Downtime::between(&spans, at(15), at(15)),
            Downtime::default()
        );
    }

    #[test]
    fn a_partial_outage_weighs_thirty_percent_and_degraded_nothing() {
        let d = Downtime {
            major_secs: 600,
            partial_secs: 1_000,
            degraded_secs: 7_200,
        };
        assert_eq!(d.weighted_secs(), 900);
    }

    /// These strings are the public API's wire contract; renaming one silently
    /// breaks every status-page client and the badge endpoint.
    #[test]
    fn component_and_day_states_serialise_as_snake_case() {
        let json = |s: PublicComponentStatus| serde_json::to_string(&s).unwrap();
        assert_eq!(json(PublicComponentStatus::Operational), "\"operational\"");
        assert_eq!(json(PublicComponentStatus::Degraded), "\"degraded\"");
        assert_eq!(
            json(PublicComponentStatus::PartialOutage),
            "\"partial_outage\""
        );
        assert_eq!(json(PublicComponentStatus::MajorOutage), "\"major_outage\"");
        assert_eq!(json(PublicComponentStatus::Maintenance), "\"maintenance\"");
        assert_eq!(json(PublicComponentStatus::NoData), "\"no_data\"");
        // The strip already spelled it this way, and the two must match.
        assert_eq!(
            serde_json::to_string(&DayState::NoData).unwrap(),
            "\"no_data\""
        );
        for impact in [
            IncidentImpact::Degraded,
            IncidentImpact::PartialOutage,
            IncidentImpact::MajorOutage,
        ] {
            assert_eq!(
                serde_json::to_string(&impact).unwrap(),
                format!("\"{}\"", impact.as_str())
            );
        }
    }
}
