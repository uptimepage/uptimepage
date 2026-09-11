use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use super::WriteSource;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum MaintenanceFilter {
    Active,
    Upcoming,
    Past,
    #[default]
    All,
}
