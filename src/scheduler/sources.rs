//! What the scheduler enumerates on a refresh tick: the control plane's own
//! region plus every passive monitor, reconciled before dispatch.

use async_trait::async_trait;
use std::sync::Arc;

use crate::domain::{OrgId, Target};
use crate::error::Result;
use crate::quotas::QuotaService;
use crate::quotas::effective;
use crate::storage::admin::{AdminRepo, EnabledTargetSource};
use crate::worker::PassiveRuntimes;
use crate::worker::heartbeat::HeartbeatRuntime;
use crate::worker::manual::ManualRuntime;

/// Scheduler source scoped to the control plane's own region. Wraps
/// [`AdminRepo`] so the local scheduler runs exactly the targets assigned to
/// its region — the same query an agent pulls for its region. Remote regions
/// are left to their agents. Passive monitors are appended: nothing is
/// probed, so the control plane evaluates all of them regardless of region.
pub struct RegionTargetSource {
    repo: AdminRepo,
    region: String,
    passive: PassiveRuntimes,
    quotas: Arc<QuotaService>,
    /// Whether this control plane runs flow in-process (single-node/self-host).
    /// Distributed deployments leave flow to a capable agent and set this `false`.
    flow_capable: bool,
}

impl RegionTargetSource {
    pub fn new(
        repo: AdminRepo,
        region: String,
        passive: PassiveRuntimes,
        quotas: Arc<QuotaService>,
        flow_capable: bool,
    ) -> Self {
        Self {
            repo,
            region,
            passive,
            quotas,
            flow_capable,
        }
    }

    async fn governed_region_targets(&self) -> Result<Vec<(OrgId, Target)>> {
        let orgs = self.repo.region_org_ids(&self.region).await?;
        let plans = effective::resolve_plans(&self.quotas, orgs).await;
        effective::region_targets(&self.repo, &self.region, self.flow_capable, &plans).await
    }
}

#[async_trait]
impl EnabledTargetSource for RegionTargetSource {
    async fn list_all_enabled_targets(&self) -> Result<Vec<(OrgId, Target)>> {
        let (mut targets, passive) = tokio::try_join!(
            self.governed_region_targets(),
            enabled_passive_synced(&self.repo, &self.passive),
        )?;
        targets.extend(passive);
        Ok(targets)
    }
}

/// Scheduler source for a control plane with in-process probing disabled
/// (agents cover every region): feeds the scheduler only the passive set,
/// which is control-plane state and must run here regardless.
pub struct PassiveTargetSource {
    repo: AdminRepo,
    passive: PassiveRuntimes,
}

impl PassiveTargetSource {
    pub fn new(repo: AdminRepo, passive: PassiveRuntimes) -> Self {
        Self { repo, passive }
    }
}

#[async_trait]
impl EnabledTargetSource for PassiveTargetSource {
    async fn list_all_enabled_targets(&self) -> Result<Vec<(OrgId, Target)>> {
        enabled_passive_synced(&self.repo, &self.passive).await
    }
}

async fn enabled_passive_synced(
    repo: &AdminRepo,
    passive: &PassiveRuntimes,
) -> Result<Vec<(OrgId, Target)>> {
    let (mut targets, manual) = tokio::try_join!(
        enabled_heartbeats_synced(repo, &passive.heartbeat),
        enabled_manual_synced(repo, &passive.manual),
    )?;
    targets.extend(manual);
    Ok(targets)
}

/// One refresh tick's heartbeat work: heal + list the rows, then reconcile the
/// ping state before the registry dispatches, so a freshly added target is
/// guaranteed resident state by ordering.
async fn enabled_heartbeats_synced(
    repo: &AdminRepo,
    runtime: &HeartbeatRuntime,
) -> Result<Vec<(OrgId, Target)>> {
    let (mut targets, states) = tokio::try_join!(
        repo.list_enabled_heartbeat_targets(),
        repo.sync_heartbeat_rows(),
    )?;
    runtime.sync_states(states.into_iter().collect());
    for (_, target) in &mut targets {
        if let Some(hb) = target.check.as_heartbeat() {
            target.interval = target.interval.min(hb.evaluation_cadence());
        }
    }
    Ok(targets)
}

/// The manual half, reconciled the same way.
async fn enabled_manual_synced(
    repo: &AdminRepo,
    runtime: &ManualRuntime,
) -> Result<Vec<(OrgId, Target)>> {
    let (targets, states) = repo.list_enabled_manual_targets().await?;
    runtime.sync_states(states.into_iter().collect());
    Ok(targets)
}
