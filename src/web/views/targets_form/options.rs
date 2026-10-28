//! The render-time option lists: channels, owners, groups, tags, plan floor.

use uuid::Uuid;

use crate::app::AppState;
use crate::domain::{OrgId, TargetAlerts};
use crate::error::AppError;

use super::model::{ChannelChoice, FormModel, OwnerChoice};
use crate::web::views::channel_kind_label;

/// The org's channels with `alerts` prefilled as the selected bindings.
/// Unbound channels default to a sensible new-binding policy.
pub(crate) async fn channel_choices(
    state: &AppState,
    org: OrgId,
    alerts: &TargetAlerts,
) -> Result<Vec<ChannelChoice>, AppError> {
    let bound: std::collections::HashSet<Uuid> = alerts.iter().map(|b| b.channel_id).collect();
    let channels = state.notification_channel_store.list(org).await?;
    Ok(channels
        .into_iter()
        .map(|c| ChannelChoice {
            selected: bound.contains(&c.id),
            id: c.id.to_string(),
            name: c.name,
            kind: channel_kind_label(c.kind),
            rule_tags: serde_json::to_string(&c.auto_bind_tags).unwrap_or_else(|_| "[]".into()),
        })
        .collect())
}

/// The org plan's check-interval floor, as the form needs it (u64 seconds).
/// Same value the API enforces via `min_check_interval`, so the client
/// `min=`/guard never disagree with the server.
pub(super) fn plan_min_interval(plan: &crate::domain::quota::Plan) -> u64 {
    u64::try_from(plan.min_check_interval_secs)
        .unwrap_or(60)
        .max(1)
}

/// Ensure the monitor's own tags appear in the option list so they always
/// render (checked) — `list_tags` is capped, so a tag outside the cap would
/// otherwise show no chip and be dropped on save.
pub(super) fn ensure_tags_listed(form: &mut FormModel) {
    let missing: Vec<String> = form
        .tags
        .iter()
        .filter(|t| !form.tag_options.contains(t))
        .cloned()
        .collect();
    form.tag_options.extend(missing);
}

/// Option lists + plan + region catalog shared by the create and edit forms.
type FormOptions = (
    Vec<ChannelChoice>,
    Vec<OwnerChoice>,
    Vec<String>,
    Vec<String>,
    std::sync::Arc<crate::domain::quota::Plan>,
    Vec<crate::storage::RegionOption>,
);

/// Fetch every independent render-time input in one `try_join!` round so their
/// latencies overlap instead of stacking. Returns channels, owner options,
/// group names, tag names, the plan, and the region catalog.
pub(super) async fn form_options(
    state: &AppState,
    org: OrgId,
    alerts: &TargetAlerts,
    owner_id: &str,
) -> Result<FormOptions, AppError> {
    let (channels, owner_options, group_options, tags, plan, regions) = tokio::try_join!(
        channel_choices(state, org, alerts),
        owner_choices(state, org, owner_id),
        state.target_store.distinct_groups(org),
        state.target_store.list_tags(org, None, 200),
        state.quotas.limit_for_org(org),
        state.regions_detailed(),
    )?;
    let tag_options = tags.into_iter().map(|t| t.name).collect();
    Ok((
        channels,
        owner_options,
        group_options,
        tag_options,
        plan,
        regions,
    ))
}

/// Org members rendered as `<select>` options for the owner field.
/// Empty option ("unowned") is added template-side.
pub(super) async fn owner_choices(
    state: &AppState,
    org: OrgId,
    selected: &str,
) -> Result<Vec<OwnerChoice>, AppError> {
    let members = match state.db.as_ref() {
        Some(pool) => crate::storage::orgs::list_members(pool, org).await?,
        None => return Ok(Vec::new()),
    };
    let mut out: Vec<OwnerChoice> = members
        .into_iter()
        .map(|m| {
            let id = m.membership.user_id.0.to_string();
            OwnerChoice {
                selected: id == selected,
                id,
                label: m.email,
            }
        })
        .collect();
    out.sort_by(|a, b| a.label.cmp(&b.label));
    Ok(out)
}
