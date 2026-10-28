//! Monitor group bodies: list them, and retune one.
//!
//! No audit here — the wrapper in [`super::tools_write`] records the outcome.

use rmcp::RoleServer;
use rmcp::handler::server::wrapper::Json;
use rmcp::service::RequestContext;

use crate::auth::scope::Scope;
use crate::domain::notification_channel::NotificationChannel;
use crate::domain::{MonitorGroup, MonitorGroupUpdate, WriteSource};
use crate::quotas::ratelimit::RateLimitCategory;

use crate::mcp::auth::McpAuth;
use crate::mcp::confirm::require_confirmation;
use crate::mcp::error::{McpToolError, codes, config_error};
use crate::mcp::schema::{
    FieldChange, MonitorGroupItem, MonitorGroupList, MonitorGroupUpdated, UpdateMonitorGroupArgs,
};

use super::McpServer;
use super::args::{bound_ids, fits_i32, parse_uuid, resolve_bindings};
use super::text::{change_lines, sanitize_data, sanitize_prompt};
use super::view::channel_names;

const SWITCH_NOTE: &str = "Only failures that start after this follow the new setting; an \
                           incident open now ends the way it started.";

impl McpServer {
    pub(super) async fn list_monitor_groups_inner(
        &self,
        auth: &McpAuth,
    ) -> Result<Json<MonitorGroupList>, McpToolError> {
        auth.require(Scope::TargetsRead)?;
        let store = &self.state.monitor_group_store;
        let to_err = |e| McpToolError::internal(format!("list monitor groups: {e}"));
        let groups = store.list(auth.org).await.map_err(to_err)?;
        let ids: Vec<uuid::Uuid> = groups.iter().map(|g| g.id).collect();
        let mut usage = store.usage(auth.org, &ids).await.map_err(to_err)?;
        Ok(Json(MonitorGroupList {
            items: groups
                .into_iter()
                .map(|g| {
                    let u = usage.remove(&g.id).unwrap_or_default();
                    MonitorGroupItem {
                        id: g.id.to_string(),
                        name: sanitize_data(&g.name),
                        alert_as_one: g.alert_as_one,
                        member_count: u.member_count,
                        open_incident_id: u.open_incident.map(|i| i.id.to_string()),
                        channel_ids: g.alerts.iter().map(|b| b.channel_id.to_string()).collect(),
                        renotify_interval_secs: g.renotify_interval_secs,
                        alert_again: g.alert_again,
                        tags: g.tags.iter().map(|t| sanitize_data(t)).collect(),
                        managed_externally: g.write_source == WriteSource::Terraform,
                    }
                })
                .collect(),
        }))
    }

    pub(super) async fn update_monitor_group_inner(
        &self,
        ctx: &RequestContext<RoleServer>,
        auth: &McpAuth,
        args: &UpdateMonitorGroupArgs,
    ) -> Result<Json<MonitorGroupUpdated>, McpToolError> {
        auth.require(Scope::TargetsWrite)?;
        self.enforce_rate_limit(auth.org, RateLimitCategory::ApiWrites)
            .await?;
        let id = parse_uuid(&args.id, "group id")?;
        let group = self.load_writable_group(auth, id).await?;
        let channels = self
            .channels_for_binding(auth, args.channel_ids.is_some())
            .await?;
        let failure_limit = self.state.cfg.escalation.channel_failure_limit;

        let (mut update, changes) = build_group_patch(args, &group, &channels, failure_limit)?;
        if changes.is_empty() {
            return Ok(Json(MonitorGroupUpdated {
                id: id.to_string(),
                changes,
            }));
        }
        self.state
            .target_ops()
            .vet_group_update(auth.org, &mut update)
            .await
            .map_err(config_error)?;

        let switch = match update.alert_as_one {
            Some(_) => format!("\n\n{SWITCH_NOTE}"),
            None => String::new(),
        };
        require_confirmation(
            ctx,
            auth,
            format!(
                "Change group \"{}\"?\n\n{}{switch}",
                sanitize_prompt(&group.name),
                change_lines(&changes)
            ),
        )
        .await?;

        // The group can move while a human reads the prompt, and the approval
        // describes the diff as it stood then.
        let current = self.load_writable_group(auth, id).await?;
        let (mut update, still) = build_group_patch(args, &current, &channels, failure_limit)?;
        if still != changes {
            return Err(McpToolError::new(
                codes::CONFLICT,
                "group changed while the change was being confirmed; read it again and retry",
                true,
            ));
        }
        self.state
            .target_ops()
            .vet_group_update(auth.org, &mut update)
            .await
            .map_err(config_error)?;
        self.state
            .monitor_group_store
            .update(auth.org, id, update, WriteSource::Api, Some(auth.user_id))
            .await
            .map_err(config_error)?
            .ok_or_else(|| McpToolError::not_found("group not found"))?;
        crate::api::handlers::groups::clear_section_pages(&self.state, auth.org, id).await;

        Ok(Json(MonitorGroupUpdated {
            id: id.to_string(),
            changes,
        }))
    }

    async fn load_writable_group(
        &self,
        auth: &McpAuth,
        id: uuid::Uuid,
    ) -> Result<MonitorGroup, McpToolError> {
        let group = self
            .state
            .monitor_group_store
            .get(auth.org, id)
            .await
            .map_err(|e| McpToolError::internal(format!("get monitor group: {e}")))?
            .ok_or_else(|| McpToolError::not_found("group not found"))?;
        if group.write_source == WriteSource::Terraform {
            return Err(McpToolError::new(
                codes::MANAGED_EXTERNALLY,
                format!(
                    "group \"{}\" is managed by Terraform. Change it in the .tf that declares \
                     it and apply, or the next `terraform apply` reverts whatever is written \
                     here.",
                    sanitize_data(&group.name)
                ),
                false,
            ));
        }
        Ok(group)
    }
}

/// The patch to apply plus one [`FieldChange`] per field that moves.
fn build_group_patch(
    args: &UpdateMonitorGroupArgs,
    group: &MonitorGroup,
    channels: &[NotificationChannel],
    failure_limit: u32,
) -> Result<(MonitorGroupUpdate, Vec<FieldChange>), McpToolError> {
    let mut update = MonitorGroupUpdate::default();
    let mut changes = Vec::new();
    let mut moved = |field: &str, from: String, to: String| {
        changes.push(FieldChange {
            field: field.to_string(),
            from,
            to,
        });
    };

    if let Some(name) = args.name.as_deref().map(str::trim)
        && name != group.name
    {
        moved("name", sanitize_data(&group.name), sanitize_data(name));
        update.name = Some(name.to_string());
    }
    if let Some(on) = args.alert_as_one
        && on != group.alert_as_one
    {
        moved(
            "alert_as_one",
            group.alert_as_one.to_string(),
            on.to_string(),
        );
        update.alert_as_one = Some(on);
    }
    if let Some(secs) = args.renotify_interval_secs
        && secs != group.renotify_interval_secs
    {
        fits_i32(u64::from(secs), "renotify_interval_secs")?;
        moved(
            "renotify_interval_secs",
            group.renotify_interval_secs.to_string(),
            secs.to_string(),
        );
        update.renotify_interval_secs = Some(secs);
    }
    if let Some(on) = args.alert_again
        && on != group.alert_again
    {
        moved("alert_again", group.alert_again.to_string(), on.to_string());
        update.alert_again = Some(on);
    }
    if let Some(ids) = args.channel_ids.as_ref() {
        let rebound = resolve_bindings(ids, channels)?;
        if bound_ids(&rebound) != bound_ids(&group.alerts) {
            let names = |a| {
                channel_names(a, &group.tags, channels, failure_limit)
                    .unwrap_or_else(|| "nobody".to_string())
            };
            moved("alerts", names(&group.alerts), names(&rebound));
            update.alerts = Some(rebound);
        }
    }
    Ok((update, changes))
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use uuid::Uuid;

    use super::*;
    use crate::domain::TargetAlerts;

    fn group() -> MonitorGroup {
        MonitorGroup {
            id: Uuid::now_v7(),
            name: "DNS".into(),
            alert_as_one: false,
            alerts: TargetAlerts::default(),
            renotify_interval_secs: 3600,
            alert_again: true,
            tags: Vec::new(),
            escalation_policy_id: None,
            write_source: WriteSource::Ui,
            created_at: Utc::now(),
            created_by: None,
            updated_at: Utc::now(),
            updated_by: None,
        }
    }

    fn args(id: Uuid) -> UpdateMonitorGroupArgs {
        UpdateMonitorGroupArgs {
            id: id.to_string(),
            name: None,
            alert_as_one: None,
            channel_ids: None,
            renotify_interval_secs: None,
            alert_again: None,
        }
    }

    #[test]
    fn only_fields_that_move_are_patched() {
        let g = group();
        let mut a = args(g.id);
        a.name = Some(" DNS ".into());
        a.alert_as_one = Some(true);
        a.alert_again = Some(true);
        let (update, changes) = build_group_patch(&a, &g, &[], 3).unwrap();
        assert_eq!(update.name, None);
        assert_eq!(update.alert_as_one, Some(true));
        assert_eq!(update.alert_again, None);
        assert_eq!(
            changes.iter().map(|c| c.field.as_str()).collect::<Vec<_>>(),
            ["alert_as_one"]
        );
    }

    #[test]
    fn a_channel_the_org_does_not_have_is_refused() {
        let g = group();
        let mut a = args(g.id);
        a.channel_ids = Some(vec![Uuid::now_v7().to_string()]);
        assert!(build_group_patch(&a, &g, &[], 3).is_err());
    }
}
