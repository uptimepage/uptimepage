//! Current status per monitor with the region quorum applied, so a page and
//! the incident writer agree on what "down" means.

use std::collections::HashMap;

use crate::domain::{CheckStatus, OrgId, RegionIncidentPolicy, Target};
use crate::storage::traits::RegionLatestStatus;
use crate::storage::{ResultsStore, TimeRange};

/// How far back the region fold reads. Long enough for every check interval to
/// have reported, short enough that a region dropped from a monitor stops
/// voting quickly.
const FOLD_LOOKBACK_HOURS: i64 = 24;

/// Fold inputs for [`folded_status`]. Heartbeats are inbound-only, so
/// a quorum over probe regions describes nothing.
pub fn folded_status_policies(
    targets: &[Target],
) -> impl Iterator<Item = (uuid::Uuid, RegionIncidentPolicy)> + '_ {
    targets
        .iter()
        .filter(|t| !t.check.is_passive())
        .map(|t| (t.id, t.region_policy))
}

/// Quorum-folded current status per monitor. Best-effort: a missing entry
/// leaves the caller on the raw `last_status` rather than failing the read.
pub async fn folded_status(
    results: &dyn ResultsStore,
    org: OrgId,
    range: TimeRange,
    policies: impl IntoIterator<Item = (uuid::Uuid, RegionIncidentPolicy)>,
) -> HashMap<uuid::Uuid, CheckStatus> {
    // Current status needs the tail of the caller's window, not all of it: a
    // region dropped from a monitor mid-window must stop voting rather than
    // freeze its last verdict into a 90-day argMax.
    let recent = TimeRange {
        from: range
            .from
            .max(range.to - chrono::Duration::hours(FOLD_LOOKBACK_HOURS)),
        to: range.to,
    };
    // Only regions that reported inside the window are in the denominator,
    // which is the rule the incident writer votes by. A region that stops
    // delivering drops out on its own; agent liveness is not consulted,
    // since the control plane's own region has no `agents` row to be live in.
    let rows = match results.latest_status_by_region(org, recent).await {
        Ok(rows) => rows,
        Err(err) => {
            tracing::warn!(error = %err, "region statuses unavailable, showing last result");
            return HashMap::new();
        }
    };
    let grouped = RegionLatestStatus::group(rows);
    policies
        .into_iter()
        .filter_map(|(id, policy)| {
            let statuses = grouped.get(&id)?;
            Some((id, policy.fold_regions(statuses.iter().copied())?))
        })
        .collect()
}
