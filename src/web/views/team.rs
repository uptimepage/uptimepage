//! /settings/team — owner-facing member + invitation management. Mutations
//! go through the JSON API; this view only reads storage for the partial.

use askama::Template;
use askama_web::WebTemplate;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, Utc};

use crate::app::AppState;
use crate::auth::invitations;
use crate::error::AppError;
use crate::storage::orgs::{self as orgs_store, MembershipStatus};
use crate::web::auth::CurrentOrg;
use crate::web::error::WebResult;
use crate::web::{Session, filters};

const TAB_TEAM: &str = "team";

#[derive(Template, WebTemplate)]
#[template(path = "settings/team.html")]
pub struct TeamPage {
    pub active_tab: &'static str,
    pub is_owner: bool,
    pub org_id: String,
}

#[derive(Template, WebTemplate)]
#[template(path = "settings/team_partial.html")]
pub struct TeamPartial {
    pub org_id: String,
    pub is_owner: bool,
    pub members: Vec<MemberRow>,
    pub invitations: Vec<InviteRow>,
    pub members_used: usize,
    pub members_limit: i64,
    pub invites_limit: i64,
}

pub struct MemberRow {
    pub user_id: String,
    pub email: String,
    pub role: &'static str,
    pub is_owner: bool,
    pub is_self: bool,
    pub joined: DateTime<Utc>,
}

pub struct InviteRow {
    pub id: String,
    pub email: String,
    pub role: String,
    pub expires: DateTime<Utc>,
}

pub async fn page(
    State(state): State<AppState>,
    org: Result<CurrentOrg, AppError>,
    session: Session,
) -> WebResult<Response> {
    let org = match super::resolve_org(org, "/settings/team") {
        Ok(o) => o,
        Err(resp) => return Ok(*resp),
    };
    let Some(user) = session.user.as_ref() else {
        return Ok(crate::web::auth::login_redirect("/settings/team").into_response());
    };
    let pool = state.require_db()?;
    let is_owner = matches!(
        orgs_store::membership_status(pool, user.id, org).await?,
        MembershipStatus::Owner
    );
    Ok(TeamPage {
        active_tab: TAB_TEAM,
        is_owner,
        org_id: org.0.to_string(),
    }
    .into_response())
}

pub async fn partial(
    State(state): State<AppState>,
    org: Result<CurrentOrg, AppError>,
    session: Session,
) -> WebResult<Response> {
    let org = match super::resolve_org(org, "/settings/team") {
        Ok(o) => o,
        Err(resp) => return Ok(*resp),
    };
    let Some(user) = session.user.as_ref() else {
        return Err(AppError::Unauthorized.into());
    };
    let pool = state.require_db()?;
    // Any active member may read the roster; non-members are cloaked as 404.
    let is_owner = match orgs_store::membership_status(pool, user.id, org).await? {
        MembershipStatus::Owner => true,
        MembershipStatus::Member => false,
        MembershipStatus::None => {
            return Err(AppError::not_found(
                crate::error::codes::ORG_NOT_FOUND,
                "organisation not found",
            )
            .into());
        }
    };

    let plan = state.quotas.limit_for_org(org).await?;
    let members: Vec<MemberRow> = orgs_store::list_members(pool, org)
        .await?
        .into_iter()
        .map(|m| MemberRow {
            user_id: m.membership.user_id.0.to_string(),
            email: m.email,
            role: m.membership.role.as_db_str(),
            is_owner: m.membership.role == crate::domain::Role::Owner,
            is_self: m.membership.user_id == user.id,
            joined: m.membership.created_at,
        })
        .collect();
    // Pending invitations are owner-management detail; members see only the roster.
    let invitations: Vec<InviteRow> = if is_owner {
        invitations::list_pending_for_org(pool, org)
            .await?
            .into_iter()
            .map(|i| InviteRow {
                id: i.id.to_string(),
                email: i.email,
                role: i.role,
                expires: i.expires_at,
            })
            .collect()
    } else {
        Vec::new()
    };

    Ok(TeamPartial {
        org_id: org.0.to_string(),
        is_owner,
        members_used: members.len(),
        members_limit: i64::from(plan.max_members),
        invites_limit: i64::from(plan.max_pending_invitations),
        members,
        invitations,
    }
    .into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn team_page_renders_invite_form_and_partial_hook_for_owner() {
        let html = TeamPage {
            active_tab: TAB_TEAM,
            is_owner: true,
            org_id: "00000000-0000-0000-0000-000000000001".into(),
        }
        .render()
        .unwrap();
        assert!(html.contains(r#"id="invite-form""#));
        // Org identity + creation live on /settings/organizations now.
        assert!(!html.contains(r#"id="org-name-form""#));
        assert!(!html.contains(r#"id="org-create-form""#));
        assert!(html.contains(r#"hx-get="/web/partials/settings/team""#));
        assert!(html.contains("send invite"));
        assert!(html.contains("team.js"));
    }

    #[test]
    fn team_page_hides_form_for_non_owner() {
        let html = TeamPage {
            active_tab: TAB_TEAM,
            is_owner: false,
            org_id: "00000000-0000-0000-0000-000000000001".into(),
        }
        .render()
        .unwrap();
        assert!(!html.contains(r#"id="invite-form""#));
        // Non-owners still get the read-only roster (the hx-get hook), just no
        // invite form or mutation script.
        assert!(html.contains(r#"hx-get="/web/partials/settings/team""#));
        assert!(!html.contains("team.js"));
        assert!(html.contains("read-only"));
    }

    #[test]
    fn team_partial_renders_rows_self_marker_and_empty_invites() {
        let html = TeamPartial {
            org_id: "00000000-0000-0000-0000-000000000001".into(),
            is_owner: true,
            members: vec![
                MemberRow {
                    user_id: "00000000-0000-0000-0000-0000000000aa".into(),
                    email: "owner@example.test".into(),
                    role: "owner",
                    is_owner: true,
                    is_self: true,
                    joined: "2026-01-01T00:00:00Z".parse().unwrap(),
                },
                MemberRow {
                    user_id: "00000000-0000-0000-0000-0000000000bb".into(),
                    email: "member@example.test".into(),
                    role: "member",
                    is_owner: false,
                    is_self: false,
                    joined: "2026-02-01T00:00:00Z".parse().unwrap(),
                },
            ],
            invitations: vec![],
            members_used: 2,
            members_limit: 5,
            invites_limit: 10,
        }
        .render()
        .unwrap();
        assert!(html.contains("owner@example.test"));
        assert!(html.contains(r#"<span class="cli-brackets font-normal">you</span>"#));
        // Self-row: leave instead of remove, and NO role toggle on yourself —
        // the self owner row must not render a "make member" button.
        assert!(html.contains("leave"));
        assert!(html.contains("make owner"));
        assert!(!html.contains("make member"));
        assert!(html.contains("# no pending invitations"));
        assert!(html.contains("2/5"));
        assert!(html.contains(r#"data-org-id="00000000-0000-0000-0000-000000000001""#));
    }

    #[test]
    fn team_partial_for_member_is_read_only() {
        let html = TeamPartial {
            org_id: "00000000-0000-0000-0000-000000000001".into(),
            is_owner: false,
            members: vec![
                MemberRow {
                    user_id: "00000000-0000-0000-0000-0000000000aa".into(),
                    email: "owner@example.test".into(),
                    role: "owner",
                    is_owner: true,
                    is_self: false,
                    joined: "2026-01-01T00:00:00Z".parse().unwrap(),
                },
                MemberRow {
                    user_id: "00000000-0000-0000-0000-0000000000bb".into(),
                    email: "me@example.test".into(),
                    role: "member",
                    is_owner: false,
                    is_self: true,
                    joined: "2026-02-01T00:00:00Z".parse().unwrap(),
                },
            ],
            invitations: vec![],
            members_used: 2,
            members_limit: 5,
            invites_limit: 10,
        }
        .render()
        .unwrap();
        // Roster is visible...
        assert!(html.contains("owner@example.test"));
        assert!(html.contains(r#"<span class="cli-brackets font-normal">you</span>"#));
        // ...but no mutation controls, and pending invitations stay owner-only.
        assert!(!html.contains("make owner"));
        assert!(!html.contains("make member"));
        assert!(!html.contains("data-team-remove"));
        assert!(!html.contains("leave"));
        assert!(!html.contains("pending invitations"));
        assert!(!html.contains("invites pending"));
    }
}
