//! Server-rendered escalation-policy pages under `/settings/escalation`: a list
//! (with the org-default selector and row actions) and a policy builder form.
//!
//! Mutations are driven from the page against the JSON API
//! (`/api/v1/escalation-policies`), so this module only renders chrome, the
//! channel choices, and prefills the builder. The org is resolved by
//! [`CurrentOrg`] exactly as the API resolves it. Editing/creating a policy is
//! owner-only — the API enforces it; a non-owner sees the page but a mutation
//! returns 403, mirroring the other owner-config surfaces.

use askama::Template;
use askama_web::WebTemplate;
use axum::extract::{Path, State};
use axum::response::{IntoResponse, Response};
use uuid::Uuid;

use crate::app::AppState;
use crate::domain::{EscalationTargetType, OrgId};
use crate::error::AppError;
use crate::request::CurrentOrg;
use crate::templates::filters;
use crate::web::error::WebResult;
use crate::web::views::resolve_org;

const TAB_ESCALATION: &str = "escalation";

pub struct PolicyRow {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub step_count: i64,
    pub repeat_count: i32,
    pub created: chrono::DateTime<chrono::Utc>,
}

/// One option in a `<select>` / checkbox set.
pub struct Choice {
    pub id: String,
    pub name: String,
    pub selected: bool,
}

#[derive(Template, WebTemplate)]
#[template(path = "settings/escalation.html")]
pub struct EscalationPage {
    pub active_tab: &'static str,
}

#[derive(Template, WebTemplate)]
#[template(path = "settings/escalation_partial.html")]
pub struct EscalationPartial {
    pub policies: Vec<PolicyRow>,
    /// Policy choices for the org-default selector (the current default marked).
    pub default_choices: Vec<Choice>,
    pub has_default: bool,
}

/// One level row in the builder, with every channel offered as a checkbox.
/// The rung number is rendered from the row's position (`loop.index`) — the JS
/// renumbers on add/remove — so the level value itself is not carried here.
pub struct LevelModel {
    pub delay_secs: i32,
    pub channels: Vec<Choice>,
    /// On-call schedules paged at this level (resolved to whoever is on call).
    pub schedules: Vec<Choice>,
}

pub struct PolicyFormModel {
    pub mode: &'static str,
    pub action: String,
    pub submit_method: &'static str,
    pub name: String,
    pub description: String,
    pub repeat_count: i32,
    /// All org channels, unchecked — the template for a freshly-added level.
    pub channel_choices: Vec<Choice>,
    /// All org on-call schedules, unchecked — for a freshly-added level.
    pub schedule_choices: Vec<Choice>,
    pub levels: Vec<LevelModel>,
    /// True when the org has no channels yet (the builder warns + links out).
    pub no_channels: bool,
}

#[derive(Template, WebTemplate)]
#[template(path = "settings/escalation_form.html")]
pub struct PolicyFormPage {
    pub active_tab: &'static str,
    pub form: PolicyFormModel,
}

pub async fn index(org: Result<CurrentOrg, AppError>) -> Response {
    match resolve_org(org, "/settings/escalation") {
        Ok(_) => EscalationPage {
            active_tab: TAB_ESCALATION,
        }
        .into_response(),
        Err(resp) => *resp,
    }
}

pub async fn list_partial(
    State(state): State<AppState>,
    org: Result<CurrentOrg, AppError>,
) -> WebResult<Response> {
    let org = match resolve_org(org, "/settings/escalation") {
        Ok(o) => o,
        Err(resp) => return Ok(*resp),
    };
    let policies = state.escalation_policy_store.list(org).await?;
    let default_id = state.escalation_policy_store.org_default(org).await?;
    let default_choices = policies
        .iter()
        .map(|p| Choice {
            id: p.id.to_string(),
            name: p.name.clone(),
            selected: Some(p.id) == default_id,
        })
        .collect();
    let rows = policies
        .into_iter()
        .map(|p| PolicyRow {
            id: p.id.to_string(),
            name: p.name,
            description: p.description,
            step_count: p.step_count,
            repeat_count: p.repeat_count,
            created: p.created_at,
        })
        .collect();
    Ok(EscalationPartial {
        policies: rows,
        default_choices,
        has_default: default_id.is_some(),
    }
    .into_response())
}

/// Channel id + name, listed once per request. Choices are built from this so
/// a policy with N levels does not re-list channels N times.
struct ChannelOption {
    id: Uuid,
    name: String,
}

async fn org_channels(state: &AppState, org: OrgId) -> WebResult<Vec<ChannelOption>> {
    Ok(state
        .notification_channel_store
        .list(org)
        .await?
        .into_iter()
        .map(|c| ChannelOption {
            id: c.id,
            name: c.name,
        })
        .collect())
}

/// Render the channel set as checkbox choices, marking `selected` ones checked.
fn choices_from(channels: &[ChannelOption], selected: &[Uuid]) -> Vec<Choice> {
    channels
        .iter()
        .map(|c| Choice {
            id: c.id.to_string(),
            name: c.name.clone(),
            selected: selected.contains(&c.id),
        })
        .collect()
}

/// Org on-call schedules, listed once per request, as the parallel target set.
async fn org_schedules(state: &AppState, org: OrgId) -> WebResult<Vec<ChannelOption>> {
    Ok(state
        .on_call_store
        .list(org)
        .await?
        .into_iter()
        .map(|s| ChannelOption {
            id: s.id,
            name: s.name,
        })
        .collect())
}

pub async fn new_form(
    State(state): State<AppState>,
    org: Result<CurrentOrg, AppError>,
) -> WebResult<Response> {
    let org = match resolve_org(org, "/settings/escalation/new") {
        Ok(o) => o,
        Err(resp) => return Ok(*resp),
    };
    let channels = org_channels(&state, org).await?;
    let schedules = org_schedules(&state, org).await?;
    let form = PolicyFormModel {
        mode: "create",
        action: "/api/v1/escalation-policies".into(),
        submit_method: "POST",
        name: String::new(),
        description: String::new(),
        repeat_count: 0,
        channel_choices: choices_from(&channels, &[]),
        schedule_choices: choices_from(&schedules, &[]),
        // One empty rung to start.
        levels: vec![LevelModel {
            delay_secs: 300,
            channels: choices_from(&channels, &[]),
            schedules: choices_from(&schedules, &[]),
        }],
        no_channels: channels.is_empty(),
    };
    Ok(PolicyFormPage {
        active_tab: TAB_ESCALATION,
        form,
    }
    .into_response())
}

pub async fn edit_form(
    State(state): State<AppState>,
    org: Result<CurrentOrg, AppError>,
    Path(id): Path<Uuid>,
) -> WebResult<Response> {
    let org = match resolve_org(org, &format!("/settings/escalation/{id}/edit")) {
        Ok(o) => o,
        Err(resp) => return Ok(*resp),
    };
    let policy = state
        .escalation_policy_store
        .get(org, id)
        .await?
        .ok_or_else(|| {
            AppError::not_found("ESCALATION_POLICY_NOT_FOUND", "escalation policy not found")
        })?;
    let channels = org_channels(&state, org).await?;
    let schedules = org_schedules(&state, org).await?;
    let mut levels: Vec<LevelModel> = policy
        .steps
        .iter()
        .map(|step| {
            let sel_channels: Vec<Uuid> = step
                .targets
                .iter()
                .filter(|t| t.target_type == EscalationTargetType::Channel)
                .filter_map(|t| t.channel_id)
                .collect();
            let sel_schedules: Vec<Uuid> = step
                .targets
                .iter()
                .filter(|t| t.target_type == EscalationTargetType::Schedule)
                .filter_map(|t| t.schedule_id)
                .collect();
            LevelModel {
                delay_secs: step.delay_secs,
                channels: choices_from(&channels, &sel_channels),
                schedules: choices_from(&schedules, &sel_schedules),
            }
        })
        .collect();
    if levels.is_empty() {
        levels.push(LevelModel {
            delay_secs: 300,
            channels: choices_from(&channels, &[]),
            schedules: choices_from(&schedules, &[]),
        });
    }
    let form = PolicyFormModel {
        mode: "edit",
        action: format!("/api/v1/escalation-policies/{}", policy.id),
        submit_method: "PATCH",
        name: policy.name,
        description: policy.description.unwrap_or_default(),
        repeat_count: policy.repeat_count,
        channel_choices: choices_from(&channels, &[]),
        schedule_choices: choices_from(&schedules, &[]),
        levels,
        no_channels: channels.is_empty(),
    };
    Ok(PolicyFormPage {
        active_tab: TAB_ESCALATION,
        form,
    }
    .into_response())
}

/// Per-monitor escalation selector choices + an "inheriting …" hint, shared by
/// the monitor form's Alerts section. The monitor's own binding is marked
/// selected; the hint shows what an unbound monitor escalates through.
pub async fn monitor_binding(
    state: &AppState,
    org: OrgId,
    target_id: Uuid,
) -> WebResult<(Vec<Choice>, String)> {
    let policies = state.escalation_policy_store.list(org).await?;
    let own = state
        .escalation_policy_store
        .target_policy(org, target_id)
        .await?;
    let choices = policies
        .iter()
        .map(|p| Choice {
            id: p.id.to_string(),
            name: p.name.clone(),
            selected: Some(p.id) == own,
        })
        .collect();
    let hint = if own.is_some() {
        String::new()
    } else {
        match state
            .escalation_policy_store
            .org_default(org)
            .await?
            .and_then(|d| policies.iter().find(|p| p.id == d))
        {
            Some(p) => format!("Inheriting the org default: {}", p.name),
            None => {
                "No escalation — pages no one until a policy is bound or an org default is set."
                    .into()
            }
        }
    };
    Ok((choices, hint))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn choice(name: &str) -> Choice {
        Choice {
            id: Uuid::nil().to_string(),
            name: name.into(),
            selected: false,
        }
    }

    #[test]
    fn list_renders_default_selector_and_rows() {
        let html = EscalationPartial {
            policies: vec![PolicyRow {
                id: "abc".into(),
                name: "Primary".into(),
                description: Some("on-call ladder".into()),
                step_count: 2,
                repeat_count: 1,
                created: "2026-06-04T12:00:00Z".parse().unwrap(),
            }],
            default_choices: vec![Choice {
                id: "abc".into(),
                name: "Primary".into(),
                selected: true,
            }],
            has_default: true,
        }
        .render()
        .unwrap();
        assert!(html.contains("Primary"));
        assert!(html.contains(r#"href="/settings/escalation/abc/edit""#));
        assert!(html.contains(r#"hx-delete="/api/v1/escalation-policies/abc""#));
        assert!(html.contains("data-default-select"));
    }

    #[test]
    fn empty_list_renders_onboarding() {
        let html = EscalationPartial {
            policies: vec![],
            default_choices: vec![],
            has_default: false,
        }
        .render()
        .unwrap();
        assert!(html.contains("No escalation policies yet"));
    }

    #[test]
    fn new_form_renders_one_empty_level() {
        let html = PolicyFormPage {
            active_tab: TAB_ESCALATION,
            form: PolicyFormModel {
                mode: "create",
                action: "/api/v1/escalation-policies".into(),
                submit_method: "POST",
                name: String::new(),
                description: String::new(),
                repeat_count: 0,
                channel_choices: vec![choice("Ops")],
                schedule_choices: vec![],
                levels: vec![LevelModel {
                    delay_secs: 300,
                    channels: vec![choice("Ops")],
                    schedules: vec![],
                }],
                no_channels: false,
            },
        }
        .render()
        .unwrap();
        assert!(html.starts_with("<!doctype html>"));
        assert!(html.contains(r#"data-action="/api/v1/escalation-policies""#));
        assert!(html.contains(r#"data-method="POST""#));
        assert!(html.contains("data-level-row"));
    }
}
