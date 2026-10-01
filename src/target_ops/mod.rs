//! What every surface that creates a monitor or probes one interactively must
//! do the same way. The REST handlers and the MCP tools both call it, so a
//! guard added here is on every front door at once.

mod guards;
mod probe;

use uuid::Uuid;

use crate::ad_hoc_dispatch::AdHocDispatch;
use crate::config::AppConfig;
use crate::domain::quota::Plan;
use crate::domain::{NewTarget, OrgId, Target, WriteSource};
use crate::error::codes;
use crate::error::{AppError, Result};
use crate::quotas::QuotaService;
use crate::security::AbuseGuard;
use crate::storage::traits::FlowRunSink;
use crate::storage::{HeartbeatStore, NotificationChannelStore, TargetStore, VariableStore};
use crate::targets::validate::{
    RegionSnapshot, canonicalize_check, validate_new_target, validate_region_policy,
};

/// Borrowed view over the stores one monitor write or interactive probe
/// touches, taken from the app state per call.
pub struct TargetOps<'a> {
    pub(crate) targets: &'a dyn TargetStore,
    pub(crate) quotas: &'a QuotaService,
    pub(crate) variables: &'a dyn VariableStore,
    pub(crate) channels: &'a dyn NotificationChannelStore,
    pub(crate) heartbeats: &'a dyn HeartbeatStore,
    pub(crate) abuse: &'a AbuseGuard,
    pub(crate) ad_hoc: &'a AdHocDispatch,
    pub(crate) flow_runs: Option<&'a dyn FlowRunSink>,
    pub(crate) cfg: &'a AppConfig,
    pub(crate) db: Option<&'a sqlx::PgPool>,
}

impl TargetOps<'_> {
    /// What every front door checks before a monitor may exist, the plan's
    /// check-interval floor among them. Flow gating, alert bindings and owner stay
    /// with the REST handler, the only caller that accepts them.
    pub async fn vet_new_target(&self, org: OrgId, new: &mut NewTarget, plan: &Plan) -> Result<()> {
        canonicalize_check(&mut new.check)?;
        validate_new_target(new, &self.ssrf_guard(), plan)?;
        let available = self.targets.available_regions().await?;
        validate_region_policy(new.region_policy, available.len())?;
        self.check_abuse(org, &new.check)?;
        self.validate_variable_refs(org, &new.check).await?;
        self.quotas.check_can_create_targets(org, None, 1).await
    }

    /// Split from `create_target` so a caller that confirms with a human can name the
    /// regions in the prompt, and so an unrunnable set is refused before any probe.
    pub async fn resolve_create_regions(
        &self,
        org: OrgId,
        new: &NewTarget,
        plan: &Plan,
        snapshot: &RegionSnapshot,
    ) -> Result<Vec<String>> {
        let check = &new.check;
        let regions = match (&new.regions, check.is_passive()) {
            (Some(_), true) => {
                return Err(AppError::unprocessable(
                    codes::REGION_INVALID,
                    "heartbeat monitors receive pings; they are not probed from regions",
                ));
            }
            (None, true) => Vec::new(),
            (Some(requested), false) => {
                let named = self.vet_requested_regions(org, requested, snapshot).await?;
                snapshot.ensure_flow_runs_in_each(check, &named)?;
                named
            }
            (None, false) => snapshot.default_for(check, plan.max_regions),
        };
        snapshot.ensure_flow_covered(check, &regions)?;
        Ok(regions)
    }

    /// Persist a vetted monitor and everything that has to exist alongside it: a
    /// heartbeat's ping row, the region set its plan pays for, and a first check so
    /// the monitor reports a state instead of sitting blank until its next tick.
    /// A caller that only writes the row leaves a monitor that cannot be pinged,
    /// probes from one region, and shows nothing. `regions` comes from
    /// `resolve_create_regions`, empty only for a heartbeat.
    pub async fn create_target(
        &self,
        org: OrgId,
        new: NewTarget,
        source: WriteSource,
        plan: &Plan,
        regions: Vec<String>,
    ) -> Result<Target> {
        let t = self
            .targets
            .create(
                org,
                new,
                source,
                i64::from(plan.max_targets),
                i64::from(plan.max_flow_checks),
            )
            .await?;
        if t.check.is_passive() {
            self.ensure_heartbeat(org, t.id).await?;
        }
        // The store seeds the deployment's default region; only this write makes a
        // set that seed does not contain stick.
        if !regions.is_empty() {
            self.targets.set_target_regions(org, t.id, &regions).await?;
        }
        self.dispatch_first_check(org, &t, &regions).await;
        Ok(t)
    }

    /// Mint (or keep) the ping-token row for a heartbeat-kind target. Its anchor
    /// becomes resident on the next scheduler refresh, the same refresh that
    /// admits the target into evaluation, so the ordering is safe.
    async fn ensure_heartbeat(&self, org: OrgId, target_id: Uuid) -> Result<()> {
        self.heartbeats
            .ensure(org, target_id)
            .await?
            .ok_or_else(|| {
                AppError::Other(anyhow::anyhow!("heartbeat row missing for {target_id}"))
            })?;
        Ok(())
    }
}
