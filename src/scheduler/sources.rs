//! What the scheduler enumerates on a refresh tick: the control plane's own
//! region plus every passive heartbeat monitor, reconciled before dispatch.

use async_trait::async_trait;
use std::sync::Arc;

use crate::domain::{OrgId, Target};
use crate::error::Result;
use crate::quotas::QuotaService;
use crate::quotas::effective;
use crate::storage::admin::{AdminRepo, EnabledTargetSource};
use crate::worker::heartbeat::HeartbeatRuntime;

/// Scheduler source scoped to the control plane's own region. Wraps
/// [`AdminRepo`] so the local scheduler runs exactly the targets assigned to
/// its region — the same query an agent pulls for its region. Remote regions
/// are left to their agents. Heartbeat monitors are appended: passive (no
/// probing), so the control plane evaluates all of them regardless of region.
pub struct RegionTargetSource {
    repo: AdminRepo,
    region: String,
    heartbeat: Arc<HeartbeatRuntime>,
    quotas: Arc<QuotaService>,
    /// Whether this control plane runs flow in-process (single-node/self-host).
    /// Distributed deployments leave flow to a capable agent and set this `false`.
    flow_capable: bool,
}

impl RegionTargetSource {
    pub fn new(
        repo: AdminRepo,
        region: String,
        heartbeat: Arc<HeartbeatRuntime>,
        quotas: Arc<QuotaService>,
        flow_capable: bool,
    ) -> Self {
        Self {
            repo,
            region,
            heartbeat,
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
        let (mut targets, heartbeats) = tokio::try_join!(
            self.governed_region_targets(),
            enabled_heartbeats_synced(&self.repo, &self.heartbeat),
        )?;
        targets.extend(heartbeats);
        Ok(targets)
    }
}

/// Scheduler source for a control plane with in-process probing disabled
/// (agents cover every region): feeds the scheduler only the passive heartbeat
/// set, which is control-plane state and must run here regardless.
pub struct HeartbeatTargetSource {
    repo: AdminRepo,
    heartbeat: Arc<HeartbeatRuntime>,
}

impl HeartbeatTargetSource {
    pub fn new(repo: AdminRepo, heartbeat: Arc<HeartbeatRuntime>) -> Self {
        Self { repo, heartbeat }
    }
}

#[async_trait]
impl EnabledTargetSource for HeartbeatTargetSource {
    async fn list_all_enabled_targets(&self) -> Result<Vec<(OrgId, Target)>> {
        enabled_heartbeats_synced(&self.repo, &self.heartbeat).await
    }
}

/// One refresh tick's heartbeat work, shared by both sources: heal + list the
/// rows, then reconcile the ping state before the registry dispatches, so a
/// freshly added target is guaranteed resident state by ordering.
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
