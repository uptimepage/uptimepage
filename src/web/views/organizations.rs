//! /settings/organizations — account-level org management. Mutations go
//! through the JSON API; this view only reads storage for the partial.

use askama::Template;
use askama_web::WebTemplate;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, Duration, Utc};

use crate::app::AppState;
use crate::domain::Role;
use crate::error::AppError;
use crate::request::Session;
use crate::storage::orgs as orgs_store;
use crate::web::error::WebResult;
use crate::web::filters;

const TAB_ORGS: &str = "organizations";
const PATH: &str = "/settings/organizations";

#[derive(Template, WebTemplate)]
#[template(path = "settings/organizations.html")]
pub struct OrganizationsPage {
    pub active_tab: &'static str,
}

#[derive(Template, WebTemplate)]
#[template(path = "settings/organizations_partial.html")]
pub struct OrganizationsPartial {
    pub orgs: Vec<OrgRow>,
    pub deleted: Vec<DeletedOrgRow>,
    pub owned_used: i64,
    pub owned_limit: i64,
    pub grace_days: u32,
}

pub struct OrgRow {
    pub id: String,
    pub slug: String,
    pub name: String,
    pub role: &'static str,
    pub is_owner: bool,
    pub is_active: bool,
}

pub struct DeletedOrgRow {
    pub id: String,
    pub slug: String,
    pub name: String,
    pub restore_by: DateTime<Utc>,
}

pub async fn page(session: Session) -> WebResult<Response> {
    if session.user.is_none() {
        return Ok(crate::request::auth::login_redirect(PATH).into_response());
    }
    Ok(OrganizationsPage {
        active_tab: TAB_ORGS,
    }
    .into_response())
}

pub async fn partial(State(state): State<AppState>, session: Session) -> WebResult<Response> {
    let Some(user) = session.user.as_ref() else {
        return Err(AppError::Unauthorized.into());
    };
    let pool = state.require_db()?;
    let grace = Duration::days(i64::from(state.cfg.tenancy.deletion_grace_period_days));

    let orgs: Vec<OrgRow> = orgs_store::list_orgs_for_user(pool, user.id)
        .await?
        .into_iter()
        .map(|r| OrgRow {
            id: r.org.id.0.to_string(),
            slug: r.org.slug,
            name: r.org.name,
            role: r.role.as_db_str(),
            is_owner: r.role == Role::Owner,
            is_active: session.active_org_id == Some(r.org.id),
        })
        .collect();

    // The store does not apply the window and the purge is daily, so an
    // expired org lingers here for up to a day with only a 422 to offer.
    let now = Utc::now();
    let deleted: Vec<DeletedOrgRow> = orgs_store::list_deleted_orgs_deleted_by(pool, user.id)
        .await?
        .into_iter()
        .filter_map(|o| {
            let restore_by = o.deleted_at? + grace;
            (restore_by > now).then(|| DeletedOrgRow {
                id: o.id.0.to_string(),
                slug: o.slug,
                name: o.name,
                restore_by,
            })
        })
        .collect();

    // Orgs are held by the account, not by the membership rows: what bounds a
    // new one is `plans.max_orgs`, and every org shares the account's one pool.
    let (owned_used, owned_limit) =
        crate::storage::accounts::org_allowance_for_user(pool, user.id).await?;

    Ok(OrganizationsPartial {
        orgs,
        deleted,
        owned_used,
        owned_limit,
        grace_days: state.cfg.tenancy.deletion_grace_period_days,
    }
    .into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(slug: &str, owner: bool, active: bool) -> OrgRow {
        OrgRow {
            id: "00000000-0000-0000-0000-000000000001".into(),
            slug: slug.into(),
            name: "Acme".into(),
            role: if owner { "owner" } else { "member" },
            is_owner: owner,
            is_active: active,
        }
    }

    fn partial(orgs: Vec<OrgRow>, deleted: Vec<DeletedOrgRow>) -> String {
        OrganizationsPartial {
            orgs,
            deleted,
            owned_used: 1,
            owned_limit: 3,
            grace_days: 30,
        }
        .render()
        .unwrap()
    }

    #[test]
    fn page_renders_create_form_and_list_hook() {
        let html = OrganizationsPage {
            active_tab: TAB_ORGS,
        }
        .render()
        .unwrap();
        assert!(html.contains(r#"id="org-create-form""#));
        assert!(html.contains(r#"hx-get="/web/partials/settings/organizations""#));
        assert!(html.contains("organizations.js"));
    }

    #[test]
    fn active_org_offers_no_switch_and_sole_org_no_delete() {
        let html = partial(vec![row("acme", true, true)], Vec::new());
        assert!(!html.contains("data-org-switch"));
        // API refuses deleting the only org; don't offer the button.
        assert!(!html.contains("data-org-delete"));
        assert!(html.contains("data-org-edit"));
    }

    #[test]
    fn second_org_offers_switch_and_delete() {
        let html = partial(
            vec![row("acme", true, true), row("client-co", true, false)],
            Vec::new(),
        );
        assert!(html.contains("data-org-switch"));
        assert!(html.contains("data-org-delete"));
        // Deleting asks for the name back, so it cannot be a stray double-click.
        assert!(html.contains(r#"data-confirm-match="client-co""#));
    }

    #[test]
    fn member_row_offers_neither_edit_nor_delete() {
        let html = partial(
            vec![row("acme", true, true), row("client-co", false, false)],
            Vec::new(),
        );
        assert_eq!(html.matches("data-org-edit").count(), 1);
        assert_eq!(html.matches("data-org-delete").count(), 1);
    }

    #[test]
    fn deleted_orgs_offer_restore_with_deadline() {
        let html = partial(
            vec![row("acme", true, true)],
            vec![DeletedOrgRow {
                id: "00000000-0000-0000-0000-000000000009".into(),
                slug: "old-co".into(),
                name: "Old Co".into(),
                restore_by: Utc::now() + Duration::days(30),
            }],
        );
        assert!(html.contains("data-org-restore"));
        assert!(html.contains("old-co"));
    }

    #[test]
    fn no_deleted_orgs_hides_the_whole_section() {
        let html = partial(vec![row("acme", true, true)], Vec::new());
        assert!(!html.contains("data-org-restore"));
        assert!(!html.contains("recently deleted"));
    }
}
