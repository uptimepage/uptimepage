//! The monitor create/edit form: the two page handlers and the page they render.

use askama::Template;
use askama_web::WebTemplate;
use axum::extract::{Path, Query, State};
use uuid::Uuid;

use crate::app::AppState;
use crate::domain::{CadenceAdvice, OrgId, RegionIncidentPolicy, TargetAlerts};
use crate::error::AppError;
use crate::request::{AuthedBrowser, CurrentOrg, CurrentUser};
use crate::templates::filters;
use crate::web::error::WebResult;

mod fields;
mod from_target;
mod model;
mod options;
mod prefill;
#[cfg(test)]
mod tests;

pub use fields::{
    CadenceHint, DnsFields, DomainExpiryFields, DurationChoice, FlowFields, FlowStepFields,
    HeaderPair, HeartbeatFields, HttpFields, PingFields, TcpFields, TlsCertFields,
};
pub use model::{
    ChannelChoice, ConfirmationChoice, FormModel, IntervalChoice, KindCard, OwnerChoice,
    RegionChoice, RegionGroup, RenotifyChoice, ThresholdChoice,
};
pub use prefill::NewParams;

use from_target::{FormKind, empty_create_form, form_from_target};
use model::{region_groups, region_threshold_choices};
use options::{ensure_tags_listed, form_options, plan_min_interval};
use prefill::{apply_kind_param, prefill_host, prefill_url};

#[derive(Template, WebTemplate)]
#[template(path = "targets/form.html")]
pub struct FormPage {
    pub active_tab: &'static str,
    pub form: FormModel,
}

impl FormPage {
    fn max_tags(&self) -> usize {
        crate::domain::target::MAX_TAGS_PER_TARGET
    }

    fn max_tag_len(&self) -> usize {
        crate::domain::target::MAX_TAG_LEN
    }
}
pub async fn new_form(
    _auth: AuthedBrowser,
    CurrentUser(user_id): CurrentUser,
    CurrentOrg(org): CurrentOrg,
    State(state): State<AppState>,
    Query(params): Query<NewParams>,
) -> WebResult<FormPage> {
    let (mut form, alerts) = match params.from {
        Some(id) => {
            let target = state
                .target_store
                .get(org, id)
                .await?
                .ok_or_else(|| AppError::not_found("TARGET_NOT_FOUND", "monitor not found"))?;
            let alerts = target.alerts.clone();
            (form_from_target(target, FormKind::Copy)?, alerts)
        }
        None => {
            let mut form = empty_create_form();
            form.owner_user_id = user_id.0.to_string();
            if let Some(kind) = params.kind.as_deref()
                && apply_kind_param(&mut form, kind)
            {
                if let Some(host) = params.host.as_deref() {
                    prefill_host(&mut form, host);
                }
                if let Some(url) = params.url.as_deref() {
                    prefill_url(&mut form, url);
                }
            }
            (form, TargetAlerts::default())
        }
    };
    let owner_id = form.owner_user_id.clone();
    let (channels, owner_options, group_options, tag_options, plan, available) =
        form_options(&state, org, &alerts, &owner_id).await?;
    form.channels = channels;
    // Otherwise the first monitor most people create alerts nobody. With
    // several channels the guess would be wrong as often as right. A copy
    // keeps the source monitor's bindings.
    if params.from.is_none()
        && let [only] = form.channels.as_mut_slice()
    {
        only.selected = true;
    }
    form.owner_options = owner_options;
    form.group_options = group_options;
    form.tag_options = tag_options;
    ensure_tags_listed(&mut form);
    form.show_escalation = state.cfg.escalation.enabled;
    form.flow_available = plan.max_flow_checks > 0;
    // `?kind=flow` and a copy both pick the kind before the plan is known.
    if form.check_type == "flow" && !form.flow_available {
        form.check_type = "http";
        form.interval_s = crate::domain::interval_hints_for_kind("http").default;
    }
    form.min_interval_s = plan_min_interval(&plan);
    // A new monitor is prefilled with 60s; raise it if the plan floor is
    // higher so the default the user sees would actually be accepted.
    form.interval_s = form.interval_s.max(form.min_interval_s);
    // Prefill the same coverage a create would seed, so the boxes match what
    // saving without touching them would do.
    let max_regions = plan.max_regions;
    if available.len() > 1 && max_regions > 1 {
        let default_region = state.cfg.scheduler.effective_default_region().to_string();
        let preferred = state.target_store.default_selected_regions().await?;
        let default_set =
            crate::targets::default_region_set(preferred, max_regions, &default_region);
        let chosen: std::collections::HashSet<String> = default_set.into_iter().collect();
        let cap = available.len().min(max_regions.max(1) as usize);
        let flow_capable = crate::targets::flow_capable_set(&state).await?;
        form.region_groups = region_groups(available, |id| chosen.contains(id), &flow_capable);
        form.region_threshold_options =
            region_threshold_choices(RegionIncidentPolicy::default(), cap);
        form.show_regions = true;
    }
    Ok(FormPage {
        active_tab: "targets",
        form,
    })
}

/// Same judgement as the monitor page, so the two cannot disagree.
async fn cadence_hint(
    state: &AppState,
    org: OrgId,
    target_id: Uuid,
    fields: &HeartbeatFields,
) -> Option<CadenceHint> {
    let observed = crate::targets::observed_cadence(state, org, target_id).await?;
    let down_after = std::time::Duration::from_secs(fields.period_s + fields.grace_s);
    let (suggested, too_tight) = match observed.advice(down_after)? {
        CadenceAdvice::TooTight { suggested_period } => (suggested_period, true),
        CadenceAdvice::TooLoose { suggested_period } => (suggested_period, false),
    };
    Some(CadenceHint {
        observed_s: observed.median_gap.as_secs(),
        suggested_s: suggested.as_secs(),
        too_tight,
    })
}

pub async fn edit_form(
    _auth: AuthedBrowser,
    CurrentOrg(org): CurrentOrg,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> WebResult<FormPage> {
    let target = state
        .target_store
        .get(org, id)
        .await?
        .ok_or_else(|| AppError::not_found("TARGET_NOT_FOUND", "monitor not found"))?;
    let alerts = target.alerts.clone();
    let region_policy = target.region_policy;
    let mut form = form_from_target(target, FormKind::Edit)?;
    let owner_id = form.owner_user_id.clone();
    let (channels, owner_options, group_options, tag_options, plan, available) =
        form_options(&state, org, &alerts, &owner_id).await?;
    form.channels = channels;
    form.owner_options = owner_options;
    form.group_options = group_options;
    form.tag_options = tag_options;
    ensure_tags_listed(&mut form);
    if form.check_type == "heartbeat" {
        form.heartbeat.cadence = cadence_hint(&state, org, id, &form.heartbeat).await;
    }
    form.show_escalation = state.cfg.escalation.enabled;
    if form.show_escalation {
        (form.escalation_choices, form.escalation_hint) =
            crate::web::views::escalation::monitor_binding(&state, org, id).await?;
    }
    // Meaningless unless the deployment has >1 region and the plan allows >1.
    let max_regions = plan.max_regions;
    if available.len() > 1 && max_regions > 1 {
        let assigned: std::collections::HashSet<String> = state
            .target_store
            .regions_for_target(org, id)
            .await?
            .unwrap_or_default()
            .into_iter()
            .collect();
        let cap = available.len().min(max_regions.max(1) as usize);
        let flow_capable = crate::targets::flow_capable_set(&state).await?;
        form.region_groups = region_groups(available, |id| assigned.contains(id), &flow_capable);
        form.region_threshold_options = region_threshold_choices(region_policy, cap);
        form.show_regions = true;
    }
    // Edit keeps the saved interval as-is; if a plan floor rose past it the
    // save will surface the API error rather than silently rewriting it.
    form.min_interval_s = plan_min_interval(&plan);
    form.flow_available = plan.max_flow_checks > 0;
    Ok(FormPage {
        active_tab: "targets",
        form,
    })
}
