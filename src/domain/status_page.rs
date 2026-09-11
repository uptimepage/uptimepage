//! Public status pages. An org owns one or more pages (plan-capped); each page
//! has its own branding + globally-unique subdomain slug and selects the
//! monitors it shows via `status_page_components`. A monitor can appear on
//! several pages, each with its own public name / group / order.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::Type;
use utoipa::ToSchema;
use uuid::Uuid;

use super::WriteSource;
use super::monitor_share::MonitorShareId;
use super::org::{OrgId, PublicOrgBranding};

/// Strongly-typed status-page id, mirroring [`OrgId`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Type, ToSchema)]
#[serde(transparent)]
#[sqlx(transparent)]
#[schema(value_type = String, format = "uuid")]
pub struct StatusPageId(pub Uuid);

impl std::fmt::Display for StatusPageId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// A resolved public page: its id plus the owning org, threaded together so
/// tenant-scoped queries never lose the org. Produced by routing resolution
/// and consumed by the public data layer.
#[derive(Debug, Clone, Copy)]
pub struct PageRef {
    pub page: StatusPageId,
    pub org: OrgId,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct StatusPage {
    pub id: StatusPageId,
    #[serde(skip)]
    pub org_id: OrgId,
    /// Globally-unique subdomain slug ({slug}.{base_domain}).
    pub slug: String,
    /// Operator-facing label in the Pages list.
    pub name: String,
    /// Published? A disabled page 404s on its public host.
    pub enabled: bool,
    #[serde(flatten)]
    pub branding: PublicOrgBranding,
    pub write_source: WriteSource,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// Set while the plan no longer covers this page. It 404s publicly and
    /// sends no subscriber mail, but stays editable in the console and comes
    /// back exactly as it was, `enabled` included.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = true)]
    pub plan_hold_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct NewStatusPage {
    pub slug: String,
    #[schema(max_length = 80)]
    pub name: String,
    #[serde(default)]
    pub enabled: bool,
}

/// Partial update of a page's identity + branding. `branding = Some` replaces
/// the display fields wholesale (logo is set via its own endpoint).
#[derive(Debug, Clone, Default, Deserialize, ToSchema)]
pub struct StatusPageUpdate {
    pub name: Option<String>,
    pub slug: Option<String>,
    pub enabled: Option<bool>,
    pub branding: Option<PublicOrgBranding>,
}

/// One monitor as it appears on a page, with per-page curation overrides.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct StatusPageComponent {
    pub target_id: Uuid,
    /// The monitor's operator-side name (for the curation UI).
    pub monitor_name: String,
    #[schema(nullable = true)]
    pub public_name: Option<String>,
    #[schema(nullable = true)]
    pub public_description: Option<String>,
    #[schema(nullable = true)]
    pub public_group: Option<String>,
    pub sort_order: i32,
    pub detail_link_enabled: bool,
    /// Kept across an untick so re-enabling returns the same URL.
    #[serde(skip)]
    pub share_id: Option<MonitorShareId>,
    /// Revoke is soft, so the flag alone would outlive the working link.
    pub share_live: bool,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct NewStatusPageComponent {
    pub target_id: Uuid,
    #[serde(default)]
    #[schema(nullable = true, max_length = 80)]
    pub public_name: Option<String>,
    #[serde(default)]
    #[schema(nullable = true, max_length = 200)]
    pub public_description: Option<String>,
    #[serde(default)]
    #[schema(nullable = true, max_length = 50)]
    pub public_group: Option<String>,
    #[serde(default)]
    pub sort_order: i32,
    #[serde(default)]
    pub detail_link_enabled: bool,
}

#[derive(Debug, Clone, Default, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct StatusPageComponentUpdate {
    #[serde(default, deserialize_with = "double_option")]
    #[schema(nullable = true, value_type = Option<String>)]
    pub public_name: Option<Option<String>>,
    #[serde(default, deserialize_with = "double_option")]
    #[schema(nullable = true, value_type = Option<String>)]
    pub public_description: Option<Option<String>>,
    #[serde(default, deserialize_with = "double_option")]
    #[schema(nullable = true, value_type = Option<String>)]
    pub public_group: Option<Option<String>>,
    pub sort_order: Option<i32>,
    pub detail_link_enabled: Option<bool>,
}

/// Lifts `Option<T>` into `Some(Option<T>)`: a missing field stays `None`
/// (leave unchanged) while an explicit JSON `null` becomes `Some(None)` (clear).
fn double_option<'de, T, D>(d: D) -> std::result::Result<Option<Option<T>>, D::Error>
where
    T: Deserialize<'de>,
    D: serde::Deserializer<'de>,
{
    Option::<T>::deserialize(d).map(Some)
}
