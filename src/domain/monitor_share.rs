//! Shareable single-monitor links. A monitor's owner mints a capability URL
//! (`/m/{token}`) that renders the monitor's read-only detail view to anyone
//! with the link — no account. The token is a 256-bit random shown once; only
//! its SHA-256 is stored. Revoke + optional expiry are the controls; a monitor
//! may have several shares.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::Type;
use utoipa::ToSchema;
use uuid::Uuid;

use super::org::OrgId;

/// Strongly-typed share id, mirroring [`StatusPageId`](super::StatusPageId).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Type, ToSchema)]
#[serde(transparent)]
#[sqlx(transparent)]
#[schema(value_type = String, format = "uuid")]
pub struct MonitorShareId(pub Uuid);

impl std::fmt::Display for MonitorShareId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// Resolved on read, never copied into the label — a rename strands a copy.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct SharePageUse {
    pub name: String,
    pub slug: String,
}

/// One share link as the operator sees it. The raw token is never here — it is
/// returned exactly once, by [`CreatedShare`].
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct MonitorShare {
    pub id: MonitorShareId,
    #[serde(skip)]
    pub org_id: OrgId,
    #[schema(value_type = String, format = "uuid")]
    pub target_id: Uuid,
    #[schema(nullable = true)]
    pub label: Option<String>,
    /// The raw capability token, for the owner to re-copy the link. `None` only
    /// when it was stored encrypted and no KEK is currently configured to
    /// decrypt it. Never leaves an owner-authenticated response.
    #[schema(nullable = true)]
    pub token: Option<String>,
    pub created_at: DateTime<Utc>,
    /// `None` = never expires.
    #[schema(nullable = true)]
    pub expires_at: Option<DateTime<Utc>>,
    /// Page views of this shared monitor since creation (live/chart polls excluded).
    pub view_count: i64,
    /// When the link was last opened, or `None` if never.
    #[schema(nullable = true)]
    pub last_viewed_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub used_by_pages: Vec<SharePageUse>,
}

/// Result of [`create`](crate::storage::MonitorShareStore::create): the stored
/// row plus the one-time plaintext token to build the link with.
#[derive(Debug, Clone)]
pub struct CreatedShare {
    pub share: MonitorShare,
    /// Raw URL token — embedded in `/m/{token}` once, never persisted.
    pub token: String,
}

/// POST body for minting a share.
#[derive(Debug, Clone, Default, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct NewMonitorShare {
    #[serde(default)]
    #[schema(nullable = true, max_length = 80)]
    pub label: Option<String>,
    /// `None` = never expires.
    #[serde(default)]
    #[schema(nullable = true)]
    pub expires_at: Option<DateTime<Utc>>,
}

/// A token resolved to its monitor, threaded with the owning org so every
/// downstream read stays tenant-scoped. The single cross-tenant-by-design
/// lookup; nothing past it widens scope.
#[derive(Debug, Clone, Copy)]
pub struct ResolvedShare {
    pub share_id: MonitorShareId,
    pub target_id: Uuid,
    pub org: OrgId,
}
