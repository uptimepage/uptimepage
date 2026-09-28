//! Org-wide operator incidents console: a management surface for the
//! operational lifecycle (acknowledge / resolve / reopen / declare / note),
//! distinct from the per-monitor incident history under `/targets/{id}`.

mod acknowledge;
mod actors;
mod console;
mod detail;
mod forms;
mod reports;
#[cfg(test)]
mod tests;

use std::collections::HashMap;

use uuid::Uuid;

use crate::app::AppState;
use crate::domain::{IncidentState, OrgId, UserId};
use crate::storage::orgs::list_members;
use crate::web::error::WebResult;

pub use acknowledge::{acknowledge, acknowledge_page};
pub(crate) use actors::ack_list;
pub use console::{list, list_partial};
pub use detail::detail;
pub use forms::{declare_form, edit_form, postmortem_form};
pub use reports::reports;

pub struct OwnerOption {
    pub value: String,
    pub label: String,
    pub selected: bool,
}

/// An incident owner rendered as a deterministic initials avatar.
#[derive(Clone)]
pub struct OwnerAvatar {
    pub initials: String,
    pub color: String,
    pub label: String,
}

fn member_avatar(u: UserId, members: &HashMap<UserId, String>) -> Option<OwnerAvatar> {
    members.get(&u).map(|email| OwnerAvatar {
        initials: crate::web::avatar::initials_from(email),
        color: crate::web::avatar::avatar_color(u.0),
        label: email.clone(),
    })
}

/// Title, else the monitor it is about.
fn incident_label(title: Option<String>, monitor_name: Option<String>) -> String {
    title
        .or(monitor_name)
        .unwrap_or_else(|| "Untitled incident".to_string())
}

fn state_label(s: IncidentState) -> &'static str {
    match s {
        IncidentState::Triggered => "triggered",
        IncidentState::Acknowledged => "acknowledged",
        IncidentState::Resolved => "resolved",
    }
}

/// User id → email label for the org, for rendering "acknowledged by …".
pub(crate) async fn members_map(
    state: &AppState,
    org: OrgId,
) -> WebResult<HashMap<UserId, String>> {
    let Some(pool) = &state.db else {
        return Ok(HashMap::new());
    };
    let members = list_members(pool, org).await?;
    Ok(members
        .into_iter()
        .map(|m| (m.membership.user_id, m.email))
        .collect())
}

/// A status page an incident with no monitor can be posted to.
pub struct PageChoice {
    pub id: String,
    pub name: String,
    pub selected: bool,
}

async fn page_choices(
    state: &AppState,
    org: OrgId,
    selected: &[Uuid],
) -> WebResult<Vec<PageChoice>> {
    Ok(state
        .status_page_store
        .list(org)
        .await?
        .into_iter()
        .map(|p| PageChoice {
            selected: selected.contains(&p.id.0),
            id: p.id.0.to_string(),
            name: p.name,
        })
        .collect())
}

/// Humanise a mean duration in seconds to a compact `1h 3m` / `5m 12s` / `8s`.
fn fmt_secs(secs: Option<f64>) -> Option<String> {
    let s = secs?.round().max(0.0) as u64;
    Some(if s >= 3600 {
        format!("{}h {}m", s / 3600, (s % 3600) / 60)
    } else if s >= 60 {
        format!("{}m {}s", s / 60, s % 60)
    } else {
        format!("{s}s")
    })
}
