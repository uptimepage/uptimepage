//! Server-rendered escalation-policy pages under `/settings/escalation`: a list
//! (with the org-default selector and row actions) and a policy builder form.
//!
//! Mutations are driven from the page against the JSON API
//! (`/api/v1/escalation-policies`), so this module only renders chrome, the
//! channel choices, and prefills the builder. The org is resolved by
//! [`CurrentOrg`] exactly as the API resolves it. Editing/creating a policy is
//! owner-only — the API enforces it; a non-owner sees the page but a mutation
//! returns 403, mirroring the other owner-config surfaces. The plan lock
//! behaves as on the on-call page.

use askama::Template;
use askama_web::WebTemplate;
use axum::extract::{Path, State};
use axum::response::{IntoResponse, Redirect, Response};
use std::collections::HashMap;
use uuid::Uuid;

use crate::app::AppState;
use crate::domain::{EscalationTargetType, OrgId, UserId};
use crate::error::AppError;
use crate::request::{AuthedBrowser, CurrentOrg, CurrentUser};
use crate::templates::filters;
use crate::templates::format::exact_duration;
use crate::web::error::WebResult;
use crate::web::views::on_call::{MemberChoice, org_members};
use crate::web::views::resolve_org;
use crate::web::views::team_lock::{TeamLock, plan_locked, shows_teaser, team_lock};

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
    pub lock: Option<TeamLock>,
}

impl EscalationPage {
    fn teaser(&self) -> bool {
        shows_teaser(&self.lock)
    }
}

#[derive(Template, WebTemplate)]
#[template(path = "settings/escalation_partial.html")]
pub struct EscalationPartial {
    pub policies: Vec<PolicyRow>,
    /// Policy choices for the org-default selector (the current default marked).
    pub default_choices: Vec<Choice>,
    pub has_default: bool,
    /// No add button above to point an empty list at.
    pub locked: bool,
}

/// One level row in the builder, with every channel offered as a checkbox.
/// The rung number is rendered from the row's position (`loop.index`) — the JS
/// renumbers on add/remove — so the level value itself is not carried here.
pub struct LevelModel {
    /// Wait before the next level, as a duration such as `5m`.
    pub delay: String,
    pub channels: Vec<Choice>,
    /// On-call schedules paged at this level (resolved to whoever is on call).
    pub schedules: Vec<Choice>,
    /// People this level pages directly, set through the API. The form shows
    /// them, flags any no page can reach, and sends back the ones kept.
    pub people: Vec<MemberChoice>,
}

pub struct PolicyFormModel {
    pub mode: &'static str,
    pub action: String,
    pub submit_method: &'static str,
    pub name: String,
    pub description: String,
    pub repeat_count: i32,
    pub levels: Vec<LevelModel>,
    /// What "add level" clones: every choice unchecked, the default wait.
    pub blank_level: LevelModel,
    /// True when the org has no channels yet (the builder warns + links out).
    pub no_channels: bool,
}

#[derive(Template, WebTemplate)]
#[template(path = "settings/escalation_form.html")]
pub struct PolicyFormPage {
    pub active_tab: &'static str,
    pub form: PolicyFormModel,
}

pub async fn index(
    _auth: AuthedBrowser,
    CurrentUser(user): CurrentUser,
    State(state): State<AppState>,
    org: Result<CurrentOrg, AppError>,
) -> WebResult<Response> {
    let org = match resolve_org(org, "/settings/escalation") {
        Ok(o) => o,
        Err(resp) => return Ok(*resp),
    };
    let lock = team_lock(&state, org, user, async {
        Ok(!state.escalation_policy_store.list(org).await?.is_empty())
    })
    .await?;
    Ok(EscalationPage {
        active_tab: TAB_ESCALATION,
        lock,
    }
    .into_response())
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
    let rows: Vec<PolicyRow> = policies
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
    let locked = rows.is_empty() && plan_locked(&state, org).await?;
    Ok(EscalationPartial {
        policies: rows,
        default_choices,
        has_default: default_id.is_some(),
        locked,
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

/// A level with nothing chosen and the default five-minute wait.
fn blank_level(channels: &[ChannelOption], schedules: &[ChannelOption]) -> LevelModel {
    LevelModel {
        delay: "5m".into(),
        channels: choices_from(channels, &[]),
        schedules: choices_from(schedules, &[]),
        people: Vec::new(),
    }
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
    if plan_locked(&state, org).await? {
        return Ok(Redirect::to("/settings/escalation").into_response());
    }
    let channels = org_channels(&state, org).await?;
    let schedules = org_schedules(&state, org).await?;
    let form = PolicyFormModel {
        mode: "create",
        action: "/api/v1/escalation-policies".into(),
        submit_method: "POST",
        name: String::new(),
        description: String::new(),
        repeat_count: 0,
        levels: vec![blank_level(&channels, &schedules)],
        blank_level: blank_level(&channels, &schedules),
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
    let members: HashMap<UserId, MemberChoice> = org_members(&state, org)
        .await?
        .into_iter()
        .map(|m| (m.id, m))
        .collect();
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
                delay: exact_duration(step.delay_secs.unsigned_abs().into()),
                channels: choices_from(&channels, &sel_channels),
                schedules: choices_from(&schedules, &sel_schedules),
                people: step
                    .targets
                    .iter()
                    .filter(|t| t.target_type == EscalationTargetType::User)
                    .filter_map(|t| members.get(&UserId(t.user_id?)).cloned())
                    .collect(),
            }
        })
        .collect();
    if levels.is_empty() {
        levels.push(blank_level(&channels, &schedules));
    }
    let form = PolicyFormModel {
        mode: "edit",
        action: format!("/api/v1/escalation-policies/{}", policy.id),
        submit_method: "PATCH",
        name: policy.name,
        description: policy.description.unwrap_or_default(),
        repeat_count: policy.repeat_count,
        levels,
        blank_level: blank_level(&channels, &schedules),
        no_channels: channels.is_empty(),
    };
    Ok(PolicyFormPage {
        active_tab: TAB_ESCALATION,
        form,
    }
    .into_response())
}

/// A monitor's escalation as its form shows it.
pub struct MonitorBinding {
    /// Every org policy, the monitor's own binding marked selected.
    pub choices: Vec<Choice>,
    /// What an unbound monitor escalates through; empty when bound.
    pub hint: String,
    /// Paged through a policy, its own or the org default.
    pub escalating: bool,
}

/// Per-monitor escalation selector choices + an "inheriting …" hint, shared by
/// the monitor form's Alerts section.
pub async fn monitor_binding(
    state: &AppState,
    org: OrgId,
    target_id: Uuid,
) -> WebResult<MonitorBinding> {
    const SIMPLE_MODE: &str =
        "No escalation policy: pages this monitor's channels, without a ladder.";
    let policies = state.escalation_policy_store.list(org).await?;
    if policies.is_empty() {
        return Ok(MonitorBinding {
            choices: Vec::new(),
            hint: SIMPLE_MODE.into(),
            escalating: false,
        });
    }
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
    if own.is_some() {
        return Ok(MonitorBinding {
            choices,
            hint: String::new(),
            escalating: true,
        });
    }
    let inherited = state
        .escalation_policy_store
        .org_default(org)
        .await?
        .and_then(|d| policies.iter().find(|p| p.id == d));
    Ok(MonitorBinding {
        choices,
        hint: inherited.map_or_else(
            || SIMPLE_MODE.into(),
            |p| format!("Inheriting the org default: {}", p.name),
        ),
        escalating: inherited.is_some(),
    })
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
            locked: false,
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
            locked: false,
        }
        .render()
        .unwrap();
        assert!(html.contains("No escalation policies yet"));
        assert!(html.contains("add one above"));
    }

    #[test]
    fn empty_locked_list_points_at_the_plan_not_a_missing_button() {
        let html = EscalationPartial {
            policies: vec![],
            default_choices: vec![],
            has_default: false,
            locked: true,
        }
        .render()
        .unwrap();
        assert!(!html.contains("add one above"));
        assert!(html.contains("Team plan"));
    }

    fn page(lock: Option<TeamLock>) -> String {
        EscalationPage {
            active_tab: TAB_ESCALATION,
            lock,
        }
        .render()
        .unwrap()
    }

    fn locked(teaser: bool) -> Option<TeamLock> {
        Some(TeamLock {
            payer: false,
            teaser,
        })
    }

    #[test]
    fn page_offers_add_policy_when_the_plan_allows() {
        let html = page(None);
        assert!(html.contains(r#"href="/settings/escalation/new""#));
        assert!(!html.contains("Team plan"));
    }

    #[test]
    fn locked_org_with_nothing_built_sees_the_pitch_not_the_page() {
        let html = page(locked(true));
        assert!(html.contains("Team plan"));
        assert!(html.contains("Ask the account owner"));
        assert!(!html.contains(r#"href="/settings/escalation/new""#));
        assert!(!html.contains("hx-get=\"/web/partials/settings/escalation\""));
    }

    #[test]
    fn downgraded_org_keeps_its_policies_under_a_notice() {
        let html = page(locked(false));
        assert!(html.contains("keep paging"));
        assert!(html.contains("hx-get=\"/web/partials/settings/escalation\""));
        assert!(!html.contains(r#"href="/settings/escalation/new""#));
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
                levels: vec![LevelModel {
                    delay: "5m".into(),
                    channels: vec![choice("Ops")],
                    schedules: vec![],
                    people: vec![],
                }],
                blank_level: LevelModel {
                    delay: "5m".into(),
                    channels: vec![choice("Ops")],
                    schedules: vec![],
                    people: vec![],
                },
                no_channels: false,
            },
        }
        .render()
        .unwrap();
        assert!(html.starts_with("<!doctype html>"));
        assert!(html.contains(r#"data-action="/api/v1/escalation-policies""#));
        assert!(html.contains(r#"data-method="POST""#));
        assert!(html.contains("data-level-row"));
        assert!(!html.contains("People paged at this level"));
    }

    #[test]
    fn a_level_keeps_the_people_it_pages() {
        let level = |people: Vec<MemberChoice>| LevelModel {
            delay: "0s".into(),
            channels: vec![choice("Ops")],
            schedules: vec![],
            people,
        };
        let html = PolicyFormPage {
            active_tab: TAB_ESCALATION,
            form: PolicyFormModel {
                mode: "edit",
                action: "/api/v1/escalation-policies/p".into(),
                submit_method: "PATCH",
                name: "Primary".into(),
                description: String::new(),
                repeat_count: 0,
                levels: vec![level(vec![
                    MemberChoice {
                        id: UserId(Uuid::from_u128(1)),
                        email: "olena@example.com".into(),
                        reachable: true,
                    },
                    MemberChoice {
                        id: UserId(Uuid::from_u128(2)),
                        email: "taras@example.com".into(),
                        reachable: false,
                    },
                ])],
                blank_level: level(vec![]),
                no_channels: false,
            },
        }
        .render()
        .unwrap();
        assert!(html.contains(&format!(r#"data-person="{}""#, Uuid::from_u128(1))));
        assert!(html.contains("olena@example.com"));
        assert!(html.contains("taras@example.com (no paging channels)"));
        assert!(html.contains("data-remove-person"));
        assert!(html.contains(r#"value="0s""#));
    }
}
