use uuid::Uuid;

use crate::domain::{
    CheckSpec, OrgId, Target, TargetAlerts, TargetUpdate, min_interval_secs_for_kind,
};
use crate::error::codes;
use crate::error::{AppError, Result};
use crate::request::state::require_pool;
use crate::security::SsrfGuard;
use crate::targets::flow_capable_set;
use crate::targets::validate::{
    RegionSnapshot, flow_covered, normalize_region_ids, validate_heartbeat_cadence,
};

use super::TargetOps;

impl TargetOps<'_> {
    pub(crate) fn ssrf_guard(&self) -> SsrfGuard {
        SsrfGuard::from_security_config(&self.cfg.security)
    }

    pub(crate) async fn region_snapshot(&self) -> Result<RegionSnapshot> {
        RegionSnapshot::load(self.targets, self.cfg).await
    }

    /// Reject a monitor whose `{{var}}` references don't all resolve against the
    /// org's variables — an unknown key or a secret used in a field that forbids it.
    /// Fails fast at save instead of silently dropping the monitor at probe time.
    /// Non-HTTP or no-variable specs are a no-op.
    pub(crate) async fn validate_variable_refs(&self, org: OrgId, check: &CheckSpec) -> Result<()> {
        use crate::domain::interpolate::{
            flow_uses_vars, repoint_risk, resolve_flow_spec, resolve_http_spec, uses_vars,
        };

        let unresolved = |e: crate::domain::interpolate::ResolveError| {
            AppError::unprocessable(codes::UNRESOLVED_VARIABLE, e.to_string())
        };
        match check {
            CheckSpec::Http(http) if uses_vars(http) => {
                let vars = self.variables.resolve_map(org).await?;
                resolve_http_spec(http, &vars)
                    .map(drop)
                    .map_err(unresolved)?;
                if let Some(risk) = repoint_risk(http, &vars) {
                    tracing::warn!(
                        org = %org.0,
                        url_variables = ?risk.url_keys,
                        secret_headers = ?risk.secret_header_keys,
                        "monitor combines a url variable with a secret header; repointing the \
                         url variable would send the secret to a different host"
                    );
                }
            }
            CheckSpec::Flow(flow) if flow_uses_vars(flow) => {
                let vars = self.variables.resolve_map(org).await?;
                resolve_flow_spec(flow, &vars)
                    .map(drop)
                    .map_err(unresolved)?;
            }
            _ => {}
        }
        Ok(())
    }

    /// Reject a flow monitor whose regions have no node that can run it — otherwise
    /// it is accepted and then silently never probed. Capable = an enabled
    /// flow-capable agent in the region, or the control plane's own region when it
    /// runs flow in-process. No-op for non-flow checks.
    pub(crate) async fn ensure_flow_regions_covered(
        &self,
        check: &CheckSpec,
        regions: &[String],
    ) -> Result<()> {
        if !matches!(check, CheckSpec::Flow(_)) {
            return Ok(());
        }
        flow_covered(
            &flow_capable_set(self.targets, self.cfg).await?,
            check,
            regions,
        )
    }

    /// Abuse admission control for one user-supplied check. Every handler that
    /// accepts a `CheckSpec` (create, bulk per item, update, test) routes through
    /// this single chokepoint, so a denylisted URL/domain can never enter the
    /// store — and every block is audited fire-and-forget to `quota_events`.
    pub(crate) fn check_abuse(&self, org: OrgId, check: &CheckSpec) -> Result<()> {
        let Some(hit) = self.abuse.inspect(check) else {
            return Ok(());
        };
        crate::quotas::service::record_quota_event(
            self.db.cloned(),
            Some(org),
            None,
            "abuse_blocked",
            Some(hit.quota_name()),
            serde_json::json!({ "detail": hit.detail }),
            None,
        );
        Err(hit.into_app_error())
    }

    /// `targets_owner_is_member_fk` refuses a non-member owner too; this turns that
    /// into a 400 naming the field instead of a constraint violation.
    pub(crate) async fn validate_owner_is_member(
        &self,
        org: OrgId,
        owner: Option<Uuid>,
    ) -> Result<()> {
        let Some(uid) = owner else { return Ok(()) };
        let members = crate::storage::orgs::list_members(require_pool(self.db)?, org).await?;
        if !members.iter().any(|m| m.membership.user_id.0 == uid) {
            return Err(AppError::bad_request_field(
                codes::OWNER_NOT_MEMBER,
                format!("owner_user_id {uid} is not a member of this org"),
                "owner_user_id",
            ));
        }
        Ok(())
    }

    /// Shared so naming regions at create time and setting them afterwards cannot
    /// drift into two ideas of a valid set. Existence before the cap: a misspelt
    /// set must not read as a quota hit, nor be audited as one.
    pub(crate) async fn vet_requested_regions(
        &self,
        org: OrgId,
        requested: &[String],
        snapshot: &RegionSnapshot,
    ) -> Result<Vec<String>> {
        let regions = normalize_region_ids(requested)?;
        if let Some(bad) = regions.iter().find(|r| !snapshot.is_available(r)) {
            return Err(AppError::unprocessable(
                codes::REGION_INVALID,
                format!("unknown or disabled region: {bad}"),
            ));
        }
        self.quotas
            .check_region_assignment(org, None, regions.len() as i64)
            .await?;
        Ok(regions)
    }

    /// Reject a binding to a channel the caller's org doesn't own (the store is
    /// org-scoped, so a foreign or deleted id resolves to `None`). Closes the
    /// IDOR where a target could otherwise reference another tenant's channel.
    pub(crate) async fn verify_alert_channels(
        &self,
        org: OrgId,
        alerts: &TargetAlerts,
    ) -> Result<()> {
        if alerts.is_empty() {
            return Ok(());
        }
        // One batched org-scoped query (mirrors maintenance's
        // `validate_component_ids`) instead of N point lookups.
        let ids: Vec<Uuid> = alerts.iter().map(|b| b.channel_id).collect();
        let known = self.channels.existing_channel_ids(org, &ids).await?;
        if let Some(missing) = ids.iter().find(|id| !known.contains(id)) {
            return Err(AppError::bad_request_field(
                codes::INVALID_ALERT_CONFIG,
                format!("notification channel {missing} does not exist"),
                "alerts.channel_id",
            ));
        }
        Ok(())
    }

    /// The PATCH counterpart of the floor check in `validate_new_target`. Either
    /// half can arrive alone, so the floor and the heartbeat pairing are judged on
    /// the merge of the request and the stored row. A heartbeat window that shrinks
    /// with no interval sent lowers the stored interval to the new cadence, rather
    /// than refusing a field the caller never named. A missing target is left for
    /// the update itself to 404.
    pub(crate) async fn validate_patch_interval(
        &self,
        org: OrgId,
        id: Uuid,
        update: &mut TargetUpdate,
        prefetched: Option<&Target>,
    ) -> Result<()> {
        let requested = update.interval.map(|i| i.as_secs() as i64);
        if requested.is_none() && update.check.is_none() {
            return Ok(());
        }
        // The row is only worth reading for the half the request leaves out. A high
        // interval answers the kind floor on its own, but never the heartbeat
        // pairing, which needs the spec to know the window it is judged against.
        let needs_row = requested.is_none() || update.check.is_none();
        let fetched = match (needs_row, prefetched) {
            (true, None) => self.targets.get(org, id).await?,
            _ => None,
        };
        let stored = prefetched.or(fetched.as_ref());
        let Some(requested) = requested.or_else(|| stored.map(|t| t.interval.as_secs() as i64))
        else {
            return Ok(());
        };
        let kind = update
            .check
            .as_ref()
            .map(|c| c.kind())
            .or_else(|| stored.map(|t| t.check.kind()));
        let plan = self.quotas.limit_for_org(org).await?;
        let plan_min = i64::from(plan.min_check_interval_secs);
        // No kind means the read was skipped, which happens only when no kind floor
        // could bind. The plan floor applies either way.
        let effective_floor =
            plan_min.max(kind.map_or(0, |k| min_interval_secs_for_kind(k) as i64));
        if requested < effective_floor {
            return Err(AppError::min_check_interval(
                requested,
                effective_floor,
                plan.id.clone(),
            ));
        }
        // Either half can arrive alone, so the pairing is judged on the merge of
        // the request and the stored row.
        let Some(check) = update.check.as_ref().or(stored.map(|t| &t.check)) else {
            return Ok(());
        };
        let mut interval = std::time::Duration::from_secs(requested.max(0) as u64);
        if update.interval.is_none()
            && let Some(hb) = check.as_heartbeat()
        {
            let cadence = hb
                .evaluation_cadence()
                .max(std::time::Duration::from_secs(effective_floor.max(0) as u64));
            if interval > cadence {
                interval = cadence;
                update.interval = Some(cadence);
            }
        }
        validate_heartbeat_cadence(check, interval, effective_floor as u64)
    }
}
