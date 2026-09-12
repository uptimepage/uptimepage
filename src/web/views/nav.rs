//! Header nav chrome as one htmx partial per page: org switcher is the main
//! swap, health pill and account identity ride back as out-of-band swaps. Slug,
//! role, and email reuse queries the page already runs — no extra per-page cost.

use askama::Template;
use askama_web::WebTemplate;
use axum::extract::State;
use axum::response::{IntoResponse, Response};

use crate::app::AppState;
use crate::storage::orgs as orgs_store;
use crate::web::Session;
use crate::web::error::WebResult;
use crate::web::views::billing::{BillingNotice, notice_for};

#[derive(Template, WebTemplate)]
#[template(path = "nav/context.html")]
pub struct NavContext {
    pub email: String,
    pub initials: String,
    pub slug: String,
    pub role: String,
    pub open_incidents: u32,
    pub show_switcher: bool,
    pub orgs: Vec<NavOrg>,
    pub billing: Option<BillingNotice>,
}

pub struct NavOrg {
    pub id: String,
    pub slug: String,
    pub is_active: bool,
}

pub async fn context(State(state): State<AppState>, session: Session) -> WebResult<Response> {
    // Session-less shells (error/expired pages): quiet empty body, not an error.
    let Some(user) = session.user.as_ref() else {
        return Ok(().into_response());
    };
    let (Some(active), Some(pool)) = (session.active_org_id, state.db.as_ref()) else {
        return Ok(().into_response());
    };
    let rows = orgs_store::list_orgs_for_user(pool, user.id).await?;
    if rows.is_empty() {
        return Ok(().into_response());
    }
    let current = rows.iter().find(|r| r.org.id == active);
    let slug = current.map_or_else(|| "—".to_string(), |r| r.org.slug.clone());
    let role = current.map_or("", |r| r.role.as_db_str()).to_string();
    // Meaningful only for a resolvable active org; a wedged session gets 0.
    let (open_incidents, billing) = match current {
        Some(_) => (
            state.open_incident_count(active).await,
            notice_for(&state, active, user.id).await,
        ),
        None => (0, None),
    };
    Ok(NavContext {
        initials: initials_of(&user.email),
        email: user.email.clone(),
        slug,
        role,
        open_incidents,
        show_switcher: rows.len() >= 2 || current.is_none(),
        orgs: rows
            .iter()
            .map(|r| NavOrg {
                id: r.org.id.0.to_string(),
                slug: r.org.slug.clone(),
                is_active: r.org.id == active,
            })
            .collect(),
        billing,
    }
    .into_response())
}

/// Avatar initials: first two alphanumerics of the email local-part.
fn initials_of(email: &str) -> String {
    let local = email.split('@').next().unwrap_or(email);
    let mut alnum = local.chars().filter(|c| c.is_alphanumeric());
    match (alnum.next(), alnum.next()) {
        (Some(a), Some(b)) => format!("{a}{b}").to_uppercase(),
        (Some(a), None) => a.to_uppercase().to_string(),
        _ => "?".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(open: u32, switcher: bool) -> NavContext {
        NavContext {
            email: "sam@acme.io".into(),
            initials: "SA".into(),
            slug: "acme".into(),
            role: "owner".into(),
            open_incidents: open,
            show_switcher: switcher,
            orgs: vec![
                NavOrg {
                    id: "00000000-0000-0000-0000-000000000001".into(),
                    slug: "acme".into(),
                    is_active: true,
                },
                NavOrg {
                    id: "00000000-0000-0000-0000-000000000002".into(),
                    slug: "client-co".into(),
                    is_active: false,
                },
            ],
            billing: None,
        }
    }

    #[test]
    fn healthy_org_shows_all_ok_pill_and_identity() {
        let html = ctx(0, true).render().unwrap();
        assert!(html.contains("nav-rail__led--ok"));
        assert!(!html.contains("nav-rail__seg--alert"));
        assert!(html.contains(r#"id="nav-identity""#));
        assert!(html.contains("sam@acme.io"));
        assert!(html.contains("SA"));
    }

    #[test]
    fn open_incidents_switch_pill_to_alert_link() {
        let html = ctx(3, true).render().unwrap();
        assert!(html.contains("nav-rail__seg--alert"));
        assert!(html.contains("3 open"));
        assert!(html.contains(r#"href="/incidents""#));
    }

    #[test]
    fn switcher_lists_orgs_and_marks_active() {
        let html = ctx(0, true).render().unwrap();
        assert!(html.contains("data-navmenu"));
        assert!(html.contains(r#"data-org-switch="00000000-0000-0000-0000-000000000002""#));
        assert_eq!(html.matches(r#"aria-current="true""#).count(), 1);
    }

    #[test]
    fn single_org_omits_slug_and_switcher() {
        let html = ctx(0, false).render().unwrap();
        assert!(!html.contains("nav-root__slug"));
        assert!(!html.contains("data-org-switch"));
    }

    #[test]
    fn initials_take_first_two_local_alnum() {
        assert_eq!(initials_of("slim@acme.io"), "SL");
        assert_eq!(initials_of("q@x.io"), "Q");
        assert_eq!(initials_of("a.b@x.io"), "AB");
    }

    #[test]
    fn a_grace_window_shows_the_days_left_and_the_fix_link_to_the_owner_only() {
        let mut c = ctx(0, false);
        c.billing = Some(BillingNotice {
            past_due_days_left: Some(9),
            pending_plan: None,
            owner: true,
        });
        let html = c.render().unwrap();
        assert!(html.contains("9 more days"));
        assert!(html.contains("/settings/billing/payment-method"));

        let mut c = ctx(0, false);
        c.billing = Some(BillingNotice {
            past_due_days_left: Some(9),
            pending_plan: None,
            owner: false,
        });
        let html = c.render().unwrap();
        assert!(html.contains("9 more days"));
        assert!(!html.contains("/settings/billing/payment-method"));
    }

    #[test]
    fn a_pending_downgrade_points_at_the_picker() {
        let mut c = ctx(0, false);
        c.billing = Some(BillingNotice {
            past_due_days_left: None,
            pending_plan: Some("Pro".into()),
            owner: true,
        });
        let html = c.render().unwrap();
        assert!(html.contains("Pro"));
        assert!(html.contains("/settings/usage"));
    }

    #[test]
    fn no_notice_leaves_the_slot_empty() {
        let html = ctx(0, false).render().unwrap();
        assert!(html.contains(r#"id="billing-notice""#));
        assert!(!html.contains("alert-card--warn"));
    }
}
