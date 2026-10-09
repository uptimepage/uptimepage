use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use super::CheckStatus;

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
    /// Most recent phase from operator updates; `investigating` if none.
    pub status_phase: IncidentStatusPhase,
    pub updates: Vec<PublicIncidentUpdate>,
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
    pub site_name: String,
    pub groups: Vec<PublicComponentGroup>,
    pub active_incidents: Vec<PublicIncident>,
    pub recent_incidents: Vec<PublicIncident>,
    /// True when the org has more incidents past `recent_incidents` than
    /// were rendered into this snapshot. Drives the "older incidents" link
    /// on the public page → archive view.
    pub recent_incidents_has_more: bool,
    pub active_maintenance: Vec<PublicMaintenance>,
    pub upcoming_maintenance: Vec<PublicMaintenance>,
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
