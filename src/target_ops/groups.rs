use std::collections::BTreeSet;

use crate::domain::{
    MonitorGroupUpdate, NewMonitorGroup, OrgId, TargetAlerts, UserId, normalize_group_name,
};
use crate::error::Result;
use crate::storage::monitor_groups::required_name;
use crate::targets::validate::{normalize_tags, validate_alerts, validate_renotify_interval};

use super::TargetOps;

impl TargetOps<'_> {
    /// The friendly refusal for writes that would create groups past the
    /// plan's cap: `names` are the groups they join by name, an existing one
    /// costing nothing.
    pub async fn check_group_room<'n>(
        &self,
        org: OrgId,
        user: Option<UserId>,
        names: impl IntoIterator<Item = &'n str>,
    ) -> Result<()> {
        let wanted: BTreeSet<&str> = names.into_iter().filter_map(normalize_group_name).collect();
        if wanted.is_empty() {
            return Ok(());
        }
        let existing = self.targets.distinct_groups(org).await?;
        let new = wanted
            .iter()
            .filter(|n| !existing.iter().any(|e| e == *n))
            .count();
        self.quotas
            .check_can_create_groups(org, user, new as i64)
            .await
    }

    /// What every front door checks on a new group before the store sees it.
    pub async fn vet_new_group(&self, org: OrgId, new: &mut NewMonitorGroup) -> Result<()> {
        new.name = required_name(&new.name)?;
        new.tags = normalize_tags(&new.tags)?;
        self.vet_group_paging(org, Some(new.renotify_interval_secs), Some(&new.alerts))
            .await
    }

    /// [`Self::vet_new_group`] for the fields an edit sends.
    pub async fn vet_group_update(
        &self,
        org: OrgId,
        update: &mut MonitorGroupUpdate,
    ) -> Result<()> {
        if let Some(name) = &update.name {
            update.name = Some(required_name(name)?);
        }
        if let Some(tags) = &update.tags {
            update.tags = Some(normalize_tags(tags)?);
        }
        self.vet_group_paging(org, update.renotify_interval_secs, update.alerts.as_ref())
            .await
    }

    async fn vet_group_paging(
        &self,
        org: OrgId,
        renotify_interval_secs: Option<u32>,
        alerts: Option<&TargetAlerts>,
    ) -> Result<()> {
        validate_renotify_interval(renotify_interval_secs)?;
        if let Some(alerts) = alerts {
            validate_alerts(alerts)?;
            self.verify_alert_channels(org, alerts).await?;
        }
        Ok(())
    }
}
