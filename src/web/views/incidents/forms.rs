//! Declaring an incident, editing one, and its postmortem.

use askama::Template;
use askama_web::WebTemplate;
use axum::extract::{Path, State};
use uuid::Uuid;

use crate::app::AppState;
use crate::domain::IncidentOrigin;
use crate::error::AppError;
use crate::error::codes;
use crate::request::{AuthedBrowser, CurrentOrg};
use crate::storage::TargetFilter;
use crate::templates::filters;
use crate::web::error::WebResult;

use super::{PageChoice, members_map, page_choices};

pub struct MonitorOption {
    pub id: String,
    pub name: String,
}

#[derive(Template, WebTemplate)]
#[template(path = "incidents/declare.html")]
pub struct DeclareIncidentPage {
    pub active_tab: &'static str,
    pub monitors: Vec<MonitorOption>,
    pub pages: Vec<PageChoice>,
}

pub async fn declare_form(
    _auth: AuthedBrowser,
    CurrentOrg(org): CurrentOrg,
    State(state): State<AppState>,
) -> WebResult<DeclareIncidentPage> {
    let targets = state
        .target_store
        .list(
            org,
            TargetFilter {
                limit: Some(10_000),
                ..Default::default()
            },
        )
        .await?;
    let monitors = targets
        .into_iter()
        .map(|t| MonitorOption {
            id: t.id.to_string(),
            name: t.name,
        })
        .collect();
    Ok(DeclareIncidentPage {
        active_tab: "incidents",
        monitors,
        pages: page_choices(&state, org, &[]).await?,
    })
}

#[derive(Template, WebTemplate)]
#[template(path = "incidents/edit.html")]
pub struct EditIncidentPage {
    pub active_tab: &'static str,
    pub id: String,
    pub title: String,
    /// Read-only: one monitor holds one open declaration, so rebinding would
    /// collide with the open-incident index.
    pub monitor_name: Option<String>,
    pub target_id: Option<String>,
    pub severity: &'static str,
    pub urgency: &'static str,
    pub visibility: &'static str,
    pub public_title: String,
    pub public_description: String,
    /// Manual origin and bound to a monitor: anything else has no uptime to move.
    pub downtime_editable: bool,
    pub counts_as_downtime: bool,
}

pub async fn edit_form(
    _auth: AuthedBrowser,
    CurrentOrg(org): CurrentOrg,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> WebResult<EditIncidentPage> {
    let inc = state
        .incident_ops_store
        .get(org, id)
        .await?
        .ok_or_else(|| AppError::not_found(codes::INCIDENT_NOT_FOUND, "incident not found"))?;
    let monitor_name = match inc.target_id {
        Some(t) => state.target_store.get(org, t).await?.map(|x| x.name),
        None => None,
    };
    // Separate read-model over the same row; the ops one carries no public copy.
    let narration = state.incident_narration_store.get(org, id).await?;
    Ok(EditIncidentPage {
        active_tab: "incidents",
        id: inc.id.to_string(),
        title: inc.title.clone().unwrap_or_default(),
        monitor_name,
        target_id: inc.target_id.map(|t| t.to_string()),
        severity: inc.severity.as_db_str(),
        urgency: inc.urgency.as_db_str(),
        visibility: inc.visibility.as_db_str(),
        public_title: narration
            .as_ref()
            .and_then(|n| n.public_title.clone())
            .unwrap_or_default(),
        public_description: narration
            .as_ref()
            .and_then(|n| n.public_description.clone())
            .unwrap_or_default(),
        downtime_editable: inc.origin == IncidentOrigin::Manual && inc.target_id.is_some(),
        counts_as_downtime: inc.counts_as_downtime,
    })
}

pub struct ActionItemModel {
    pub text: String,
    pub owner_user_id: String,
    pub done: bool,
}

pub struct MemberChoice {
    pub id: String,
    pub email: String,
}

#[derive(Template, WebTemplate)]
#[template(path = "incidents/postmortem_form.html")]
pub struct PostmortemFormPage {
    pub active_tab: &'static str,
    pub incident_id: String,
    pub incident_label: String,
    pub exists: bool,
    pub published: bool,
    pub summary: String,
    pub root_cause: String,
    pub impact: String,
    pub action_items: Vec<ActionItemModel>,
    pub members: Vec<MemberChoice>,
}

pub async fn postmortem_form(
    _auth: AuthedBrowser,
    CurrentOrg(org): CurrentOrg,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> WebResult<PostmortemFormPage> {
    let inc = state
        .incident_ops_store
        .get(org, id)
        .await?
        .ok_or_else(|| AppError::not_found(codes::INCIDENT_NOT_FOUND, "incident not found"))?;
    let monitor_name = match inc.target_id {
        Some(t) => state.target_store.get(org, t).await?.map(|x| x.name),
        None => None,
    };
    let incident_label = inc
        .title
        .clone()
        .or(monitor_name)
        .unwrap_or_else(|| "Untitled incident".to_string());

    let pm = state.postmortem_store.get(org, id).await?;
    let members: Vec<MemberChoice> = members_map(&state, org)
        .await?
        .into_iter()
        .map(|(uid, email)| MemberChoice {
            id: uid.to_string(),
            email,
        })
        .collect();

    let action_items = pm
        .as_ref()
        .map(|p| {
            p.action_items
                .iter()
                .map(|a| ActionItemModel {
                    text: a.text.clone(),
                    owner_user_id: a.owner_user_id.map(|u| u.to_string()).unwrap_or_default(),
                    done: a.done,
                })
                .collect()
        })
        .unwrap_or_default();

    Ok(PostmortemFormPage {
        active_tab: "incidents",
        incident_id: id.to_string(),
        incident_label,
        exists: pm.is_some(),
        published: pm.as_ref().is_some_and(|p| p.published_at.is_some()),
        summary: pm
            .as_ref()
            .and_then(|p| p.summary.clone())
            .unwrap_or_default(),
        root_cause: pm
            .as_ref()
            .and_then(|p| p.root_cause.clone())
            .unwrap_or_default(),
        impact: pm
            .as_ref()
            .and_then(|p| p.impact.clone())
            .unwrap_or_default(),
        action_items,
        members,
    })
}
