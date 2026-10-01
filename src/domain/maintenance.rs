use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use super::{UserId, WriteSource};

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct MaintenanceWindow {
    pub id: Uuid,
    pub title: String,
    #[schema(nullable = true)]
    pub description: Option<String>,
    pub starts_at: DateTime<Utc>,
    pub ends_at: DateTime<Utc>,
    pub component_ids: Vec<Uuid>,
    /// Whether the window silences paging for its components while it runs.
    pub suppress_alerts: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// Where this window was last changed from (UI, API, or Terraform).
    #[serde(default)]
    pub write_source: WriteSource,
    /// Who scheduled the window. Empty once that user is removed.
    #[serde(default)]
    #[schema(nullable = true)]
    pub created_by: Option<UserId>,
    /// Who changed the window last.
    #[serde(default)]
    #[schema(nullable = true)]
    pub updated_by: Option<UserId>,
    /// Set when the window was cancelled. A cancelled window is history: it no
    /// longer shows publicly, holds paging, or counts toward the quota.
    #[serde(default)]
    #[schema(nullable = true)]
    pub deleted_at: Option<DateTime<Utc>>,
    /// Who cancelled the window.
    #[serde(default)]
    #[schema(nullable = true)]
    pub deleted_by: Option<UserId>,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct NewMaintenanceWindow {
    #[schema(example = "Database upgrade", max_length = 200)]
    pub title: String,
    #[serde(default)]
    #[schema(nullable = true)]
    pub description: Option<String>,
    pub starts_at: DateTime<Utc>,
    pub ends_at: DateTime<Utc>,
    /// IDs of targets affected by this maintenance.
    #[serde(default)]
    pub component_ids: Vec<Uuid>,
    /// Whether the window silences paging for its components while it runs.
    /// Defaults to true.
    #[serde(default = "default_true")]
    pub suppress_alerts: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Default, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct MaintenanceWindowUpdate {
    pub title: Option<String>,
    pub description: Option<String>,
    pub starts_at: Option<DateTime<Utc>>,
    pub ends_at: Option<DateTime<Utc>>,
    pub component_ids: Option<Vec<Uuid>>,
    pub suppress_alerts: Option<bool>,
}

/// Where a window is in its life, for code holding a loaded window. The SQL that
/// classifies rows in the database spells out the same boundaries and has to
/// change with them: `storage::maintenance` builds its filters, the suppression
/// check, cancel and end from one set of helpers, while the quota count, the
/// public read and the subscriber fan-out carry their own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowPhase {
    Upcoming,
    Active,
    Completed,
    Cancelled,
}

impl WindowPhase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Upcoming => "upcoming",
            Self::Active => "active",
            Self::Completed => "completed",
            Self::Cancelled => "cancelled",
        }
    }

    /// Completed and cancelled windows are history.
    pub fn is_closed(self) -> bool {
        matches!(self, Self::Completed | Self::Cancelled)
    }
}

impl MaintenanceWindow {
    pub fn phase(&self, now: DateTime<Utc>) -> WindowPhase {
        if self.deleted_at.is_some() {
            WindowPhase::Cancelled
        } else if self.ends_at <= now {
            WindowPhase::Completed
        } else if self.starts_at <= now {
            WindowPhase::Active
        } else {
            WindowPhase::Upcoming
        }
    }
}

impl MaintenanceWindowUpdate {
    /// Names of the fields this update sets, for the audit trail.
    pub fn changed_fields(&self) -> Vec<&'static str> {
        [
            ("title", self.title.is_some()),
            ("description", self.description.is_some()),
            ("starts_at", self.starts_at.is_some()),
            ("ends_at", self.ends_at.is_some()),
            ("component_ids", self.component_ids.is_some()),
            ("suppress_alerts", self.suppress_alerts.is_some()),
        ]
        .into_iter()
        .filter_map(|(name, set)| set.then_some(name))
        .collect()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum MaintenanceFilter {
    /// Running now and not cancelled.
    Active,
    /// Not started yet and not cancelled.
    Upcoming,
    /// Ended or cancelled.
    Past,
    /// Every window, cancelled ones included.
    #[default]
    All,
}
