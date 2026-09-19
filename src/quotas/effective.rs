//! Plan ceilings applied to handed-out work. Never written back to the row.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

use crate::domain::{OrgId, Plan, Target, min_interval_secs_for_kind};
use crate::error::Result;
use crate::quotas::QuotaService;
use crate::storage::admin::{AdminRepo, EnabledTargetStream, PublicTargetCursor};

pub type PlanMap = HashMap<OrgId, Option<Arc<Plan>>>;

/// The same floor the write path enforces: a row stored below its kind's
/// minimum predates that minimum, and handing it out unclamped would probe at
/// a rate the API now refuses to create.
pub fn governed_interval(requested: Duration, plan: &Plan, kind: &str) -> Duration {
    let plan_floor = u64::try_from(plan.min_check_interval_secs).unwrap_or(0);
    let floor = Duration::from_secs(plan_floor.max(min_interval_secs_for_kind(kind)));
    requested.max(floor)
}

/// Infallible: one org whose plan will not resolve must not stop the batch,
/// which spans every tenant. A monitor running at its stored rate is a far
/// smaller fault than one that stops.
pub async fn resolve_plans(
    quotas: &QuotaService,
    orgs: impl IntoIterator<Item = OrgId>,
) -> PlanMap {
    let mut plans = PlanMap::new();
    for org in orgs {
        if plans.contains_key(&org) {
            continue;
        }
        let plan = match quotas.limit_for_org(org).await {
            Ok(plan) => Some(plan),
            Err(err) => {
                tracing::warn!(
                    org_id = %org.0,
                    error = %err,
                    "plan lookup failed; running this org's monitors as stored"
                );
                None
            }
        };
        plans.insert(org, plan);
    }
    plans
}

pub async fn govern(quotas: &QuotaService, targets: &mut [(OrgId, Target)]) {
    let orgs: Vec<OrgId> = targets.iter().map(|(org, _)| *org).collect();
    let plans = resolve_plans(quotas, orgs).await;
    govern_with(&plans, targets);
}

/// For a caller that already resolved the plans, so the same resolution can
/// also decide whether the work needed sending at all.
pub fn govern_with(plans: &PlanMap, targets: &mut [(OrgId, Target)]) {
    for (org, target) in targets.iter_mut() {
        // A heartbeat's interval is the schedule its sender promised, not a
        // probe rate we pay for; slowing it only delays the missed-ping alarm.
        if target.check.is_passive() {
            continue;
        }
        if let Some(Some(plan)) = plans.get(org) {
            target.interval = governed_interval(target.interval, plan, target.check.kind());
        }
    }
}

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

/// One region's share of the work, under the plans the caller resolved: the
/// monitors whose region set reaches this region within the plan's cap, each
/// at its governed interval. The scheduler's own region and an agent's pull
/// both come through here so neither can be handed what the other refuses.
pub async fn region_targets(
    repo: &AdminRepo,
    region: &str,
    flow_capable: bool,
    plans: &PlanMap,
) -> Result<Vec<(OrgId, Target)>> {
    let mut targets = repo
        .list_enabled_targets_for_region(region, flow_capable, &RegionCaps::from(plans))
        .await?;
    govern_with(plans, &mut targets);
    Ok(targets)
}

/// Stable over the inputs that move an interval or drop a region, so a tier
/// change invalidates a cached pull that no target row has touched.
pub fn plan_digest(plans: &PlanMap) -> String {
    let mut parts: Vec<String> = plans
        .iter()
        .map(|(org, plan)| match plan {
            Some(p) => format!(
                "{}:{}:{}:{}",
                org.0, p.id, p.min_check_interval_secs, p.max_regions
            ),
            None => format!("{}:?", org.0),
        })
        .collect();
    parts.sort_unstable();
    crate::security::sha256_hex(&parts.join(","))
}

/// Applies the plan floor to the incident writer's walk over enabled targets.
/// It must see the same interval the scheduler runs: the writer sizes its
/// lookback window from it, and a window sized to the stored rate cannot hold
/// enough results from a monitor the plan has slowed down.
pub struct PlanGoverned<S: ?Sized> {
    inner: Arc<S>,
    quotas: Arc<QuotaService>,
}

impl<S: ?Sized> PlanGoverned<S> {
    pub fn new(inner: Arc<S>, quotas: Arc<QuotaService>) -> Self {
        Self { inner, quotas }
    }
}

#[async_trait]
impl<S: EnabledTargetStream + ?Sized> EnabledTargetStream for PlanGoverned<S> {
    async fn next_enabled_target_page(
        &self,
        after: Option<PublicTargetCursor>,
        limit: usize,
    ) -> Result<Vec<(OrgId, Target)>> {
        let mut targets = self.inner.next_enabled_target_page(after, limit).await?;
        govern(&self.quotas, &mut targets).await;
        Ok(targets)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::quotas::service::unlimited_plan;

    fn plan_with_floor(secs: i32) -> Plan {
        Plan {
            min_check_interval_secs: secs,
            ..unlimited_plan()
        }
    }

    #[test]
    fn a_monitor_slower_than_the_floor_keeps_its_own_interval() {
        let plan = plan_with_floor(60);
        let got = governed_interval(Duration::from_secs(300), &plan, "http");
        assert_eq!(got, Duration::from_secs(300));
    }

    #[test]
    fn a_monitor_faster_than_the_floor_is_slowed_to_it() {
        let plan = plan_with_floor(180);
        let got = governed_interval(Duration::from_secs(60), &plan, "http");
        assert_eq!(got, Duration::from_secs(180));
    }

    #[test]
    fn a_floor_equal_to_the_interval_changes_nothing() {
        let plan = plan_with_floor(60);
        let got = governed_interval(Duration::from_secs(60), &plan, "http");
        assert_eq!(got, Duration::from_secs(60));
    }

    #[test]
    fn the_unlimited_plan_imposes_no_floor() {
        let got = governed_interval(Duration::from_secs(10), &unlimited_plan(), "http");
        assert_eq!(got, Duration::from_secs(10));
    }

    #[test]
    fn a_kind_floor_above_the_plan_floor_wins() {
        let plan = plan_with_floor(30);
        let got = governed_interval(Duration::from_secs(3600), &plan, "domain_expiry");
        assert_eq!(got, Duration::from_secs(43_200));
    }

    #[test]
    fn a_plan_floor_above_the_kind_floor_still_wins() {
        let plan = plan_with_floor(180);
        let got = governed_interval(Duration::from_secs(60), &plan, "http");
        assert_eq!(got, Duration::from_secs(180));
    }

    #[test]
    fn a_negative_floor_is_treated_as_no_floor() {
        let plan = plan_with_floor(-30);
        let got = governed_interval(Duration::from_secs(45), &plan, "http");
        assert_eq!(got, Duration::from_secs(45));
    }
}
