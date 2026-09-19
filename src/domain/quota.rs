use std::collections::HashMap;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::domain::org::OrgId;
use crate::domain::user::UserId;

/// The `plans` model. Flat mirror of the table columns so call sites read
/// `plan.max_targets` without an indirection. Integer columns are `i32` to
/// mirror Postgres `INTEGER` faithfully; the non-zero / arithmetic-safe
/// wrappers belong to the enforcement layer, not the row type. The storage
/// layer owns the `sqlx::FromRow` mapping (a `PlanRow`) per the
/// domain/storage split used elsewhere in this codebase.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Plan {
    pub id: String,
    pub name: String,
    pub description: String,

    // Resource quotas
    pub max_targets: i32,
    pub min_check_interval_secs: i32,
    pub retention_days: i32,
    pub raw_days: i32,
    /// Days a failed flow run keeps its page snapshot, inside the `raw_days`
    /// window the run itself lives in.
    pub evidence_days: i32,
    pub max_members: i32,
    pub max_pending_invitations: i32,
    pub max_api_tokens_per_user: i32,
    pub max_public_components: i32,
    pub max_status_pages: i32,
    pub max_share_links_per_monitor: i32,
    pub max_shared_monitors: i32,
    pub max_maintenance_windows: i32,
    pub max_notification_channels: i32,
    pub max_escalation_policies: i32,
    pub max_on_call_schedules: i32,
    pub max_logo_size_bytes: i32,
    pub max_regions: i32,
    /// Orgs one account may hold. Orgs are workspaces over one shared pool of
    /// caps, so this bounds sprawl, not capacity.
    pub max_orgs: i32,

    // Per-org rate limits (per minute)
    pub api_writes_per_minute: i32,
    pub api_reads_per_minute: i32,
    pub bulk_ops_per_minute: i32,
    pub test_now_per_minute: i32,
    pub check_now_per_minute: i32,

    // Feature toggles
    pub custom_domain_enabled: bool,
    pub white_label_enabled: bool,
    pub sms_alerts_enabled: bool,
    pub incident_narration_enabled: bool,
    pub on_call_enabled: bool,
    /// Cap on browser-flow monitors; 0 both disables the kind and gates it.
    pub max_flow_checks: i32,
    /// Steps one flow monitor may declare, clamped to the engine ceiling.
    pub max_flow_steps: i32,

    // Metadata
    pub is_listed: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl Plan {
    /// The resource-quota subset. The override-merge and the usage endpoint
    /// only care about these caps, not the rate-limit / metadata columns.
    pub fn limits(&self) -> PlanLimits {
        PlanLimits {
            max_targets: self.max_targets,
            min_check_interval_secs: self.min_check_interval_secs,
            retention_days: self.retention_days,
            max_members: self.max_members,
            max_pending_invitations: self.max_pending_invitations,
            max_api_tokens_per_user: self.max_api_tokens_per_user,
            max_public_components: self.max_public_components,
            max_status_pages: self.max_status_pages,
            max_maintenance_windows: self.max_maintenance_windows,
            max_notification_channels: self.max_notification_channels,
            max_logo_size_bytes: self.max_logo_size_bytes,
        }
    }

    /// Raw + 1m rollup physical retention (days): raw forensics and
    /// minute-resolution history cap here.
    pub const RAW_MAX_DAYS: i64 = 90;
    /// 1h rollup physical retention (days) — the long history tail.
    pub const HISTORY_MAX_DAYS: i64 = 395;

    pub fn history_window_days(&self) -> i64 {
        i64::from(self.retention_days).min(Self::HISTORY_MAX_DAYS)
    }

    pub fn raw_window_days(&self) -> i64 {
        i64::from(self.raw_days).min(Self::RAW_MAX_DAYS)
    }
}

/// Physical per-row TTL (days) for a raw-retention value: the same ceiling as
/// [`Plan::raw_window_days`], floored at 1 so a 0 or negative value never means
/// "delete on the next merge". Single source for the write-path TTL stamp.
pub fn raw_ttl_days(raw_days: i32) -> u16 {
    i64::from(raw_days).clamp(1, Plan::RAW_MAX_DAYS) as u16
}

/// Physical TTL (days) for a failed flow run's page snapshot. Clamped to the
/// row's own window: outliving the run that owns it would leave page content
/// with nothing to explain, which is the opposite of why it is kept.
pub fn evidence_ttl_days(evidence_days: i32, raw_days: i32) -> u16 {
    let row = raw_ttl_days(raw_days);
    i64::from(evidence_days).clamp(1, i64::from(row)) as u16
}

#[cfg(test)]
mod tests {
    use super::{Plan, evidence_ttl_days, raw_ttl_days};

    #[test]
    fn raw_ttl_days_floors_at_one_and_caps_at_max() {
        let max = Plan::RAW_MAX_DAYS as u16;
        assert_eq!(raw_ttl_days(30), 30);
        assert_eq!(raw_ttl_days(0), 1, "0 days would delete on next merge");
        assert_eq!(raw_ttl_days(-5), 1);
        assert_eq!(raw_ttl_days(Plan::RAW_MAX_DAYS as i32), max);
        assert_eq!(
            raw_ttl_days(10_000),
            max,
            "never retain past the disclosed max"
        );
        assert_eq!(raw_ttl_days(i32::MAX), max);
    }

    #[test]
    fn evidence_ttl_days_never_outlives_the_row_it_explains() {
        assert_eq!(evidence_ttl_days(7, 30), 7);
        assert_eq!(evidence_ttl_days(0, 30), 1);
        assert_eq!(evidence_ttl_days(-5, 30), 1);
        // A plan configured to keep evidence longer than the run is clamped,
        // not honoured — the row is gone either way.
        assert_eq!(evidence_ttl_days(90, 30), 30);
        assert_eq!(evidence_ttl_days(i32::MAX, 30), 30);
        // The row's own ceiling still applies underneath.
        assert_eq!(
            evidence_ttl_days(i32::MAX, i32::MAX),
            Plan::RAW_MAX_DAYS as u16
        );
    }
}

/// The resource caps, grouped. Carrying these as one value keeps the future
/// per-org override merge a single step instead of a dozen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct PlanLimits {
    pub max_targets: i32,
    pub min_check_interval_secs: i32,
    pub retention_days: i32,
    pub max_members: i32,
    pub max_pending_invitations: i32,
    pub max_api_tokens_per_user: i32,
    pub max_public_components: i32,
    pub max_status_pages: i32,
    pub max_maintenance_windows: i32,
    pub max_notification_channels: i32,
    pub max_logo_size_bytes: i32,
}

impl From<&Plan> for PlanLimits {
    fn from(p: &Plan) -> Self {
        p.limits()
    }
}

/// The append-only `quota_events` audit model. `user_id` is nullable so the
/// audit row survives user deletion (`ON DELETE SET NULL`); `org_id` is
/// nullable for events with no owning org and is `ON DELETE CASCADE` (a
/// hard-deleted org takes its quota events with it). The storage layer owns
/// the `sqlx::FromRow` mapping per the domain/storage split.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuotaEvent {
    pub id: Uuid,
    pub org_id: Option<OrgId>,
    pub user_id: Option<UserId>,
    /// `quota_exceeded` | `rate_limited` | `abuse_blocked`.
    pub event: String,
    /// Column name from `plans` or the rate-limit category; `None` for
    /// events not tied to a single named quota.
    pub quota_name: Option<String>,
    pub details: serde_json::Value,
    pub ip_hash: Option<String>,
    pub occurred_at: DateTime<Utc>,
}

/// The two physical windows a written row is stamped with: how long the row
/// lives, and how long a failed flow run's page snapshot lives inside it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetentionDays {
    pub row: u16,
    pub evidence: u16,
}

/// Cache-key tags for the usage cache. One vocabulary, equal to the
/// `plans` column names so the transparency endpoint, the UI, and any
/// future invalidation hook all name a quota the same way.
pub mod usage_keys {
    pub const TARGETS: &str = "max_targets";
    pub const MEMBERS: &str = "max_members";
    pub const PENDING_INVITATIONS: &str = "max_pending_invitations";
    /// Not a `plans` column: an abuse ceiling, equal for every plan.
    pub const INVITATION_SENDS: &str = "invitation_sends_per_window";
    pub const PUBLIC_COMPONENTS: &str = "max_public_components";
    pub const STATUS_PAGES: &str = "max_status_pages";
    pub const MAINTENANCE_WINDOWS: &str = "max_maintenance_windows";
    pub const NOTIFICATION_CHANNELS: &str = "max_notification_channels";
    pub const ESCALATION_POLICIES: &str = "max_escalation_policies";
    pub const ON_CALL_SCHEDULES: &str = "max_on_call_schedules";
    pub const FLOW_CHECKS: &str = "max_flow_checks";
    pub const ORGS: &str = "max_orgs";
}

/// Every org's resolved plan, absent where resolution failed.
pub type PlanMap = HashMap<OrgId, Option<Arc<Plan>>>;

/// Per-org ceiling on how many of a monitor's regions are probed. An org whose
/// plan did not resolve is absent, which the query reads as no ceiling.
///
/// The two arrays are bound to a single `unnest`, which pads the shorter one
/// with NULLs rather than erroring — a length that drifted would silently lift
/// the ceiling for whichever orgs fell off the end. They are private and only
/// [`RegionCaps::from`] fills them, so the lengths cannot disagree.
#[derive(Debug, Default, Clone)]
pub struct RegionCaps {
    org_ids: Vec<uuid::Uuid>,
    limits: Vec<i32>,
}

impl RegionCaps {
    /// The org ids and their ceilings, positionally paired for `unnest`.
    pub fn arrays(&self) -> (&[uuid::Uuid], &[i32]) {
        (&self.org_ids, &self.limits)
    }
}

impl From<&PlanMap> for RegionCaps {
    fn from(plans: &PlanMap) -> Self {
        let mut caps = Self::default();
        for (org, plan) in plans {
            if let Some(plan) = plan {
                caps.org_ids.push(org.0);
                caps.limits.push(plan.max_regions);
            }
        }
        caps
    }
}
