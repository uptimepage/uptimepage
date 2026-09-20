use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

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

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct PublicComponent {
    pub id: Uuid,
    pub name: String,
    #[schema(nullable = true)]
    pub description: Option<String>,
    pub current_status: PublicComponentStatus,
    /// Daily history, oldest first.
    pub history: Vec<DayState>,
    /// Confirmed-incident downtime over the time the component was probed
    /// within the history span, as a percentage; `null` until it is probed.
    #[serde(default)]
    #[schema(nullable = true)]
    pub uptime_pct: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = true)]
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
}

/// Title of an incident whose operator never set `public_title`: the
/// component plus the status it opened in, as in `"API major outage"`.
pub fn auto_incident_title(component_name: &str, status_at_start: &str) -> String {
    format!("{} {}", component_name, status_at_start.replace('_', " "))
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct PublicIncident {
    pub id: Uuid,
    pub component_id: Uuid,
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
    }
}
