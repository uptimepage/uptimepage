//! Renders the help form. The submit runs against `/api/v1/support`, which
//! owns validation, rate limiting and delivery.

use askama::Template;
use askama_web::WebTemplate;
use axum::response::{IntoResponse, Response};

use crate::error::AppError;
use crate::request::CurrentOrg;
use crate::request::Session;
use crate::templates::filters;
use crate::web::views::resolve_org;

const TAB_HELP: &str = "help";

#[derive(Template, WebTemplate)]
#[template(path = "help.html")]
pub struct HelpPage {
    pub active_tab: &'static str,
    /// Shown back so a wrong address is caught before they wait on an answer.
    pub reply_to: String,
}

pub async fn page(session: Session, org: Result<CurrentOrg, AppError>) -> Response {
    if let Err(resp) = resolve_org(org, "/help") {
        return *resp;
    }
    HelpPage {
        active_tab: TAB_HELP,
        reply_to: session
            .user
            .as_ref()
            .map_or_else(String::new, |u| u.email.clone()),
    }
    .into_response()
}
