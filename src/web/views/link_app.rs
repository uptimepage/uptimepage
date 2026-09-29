//! `/link/{app}`, where the offer sent to a Pushover or Slack account that
//! acknowledged without a name lands. Signing in is what names the person, so
//! the page asks for a session first. It reads the offer only to say early that
//! it lapsed; the code is spent by the API call the page makes.

use askama::Template;
use askama_web::WebTemplate;
use axum::extract::{OriginalUri, Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

use crate::app::AppState;
use crate::domain::LinkedApp;
use crate::request::auth::{Session, login_redirect};
use crate::security::sha256_hex;
use crate::templates::filters;
use crate::web::error::WebResult;

#[derive(Debug, Deserialize)]
pub struct LinkQuery {
    #[serde(default)]
    pub c: String,
}

#[derive(Template, WebTemplate)]
#[template(path = "link_app.html")]
pub struct LinkAppPage {
    pub live: bool,
    pub email: String,
    pub app: &'static str,
    pub slug: &'static str,
    /// What the app called the account when it pressed.
    pub account: Option<String>,
    pub code: String,
    /// What the linked account's presses name the person on.
    pub presses: &'static str,
    /// How a lapsed link is replaced.
    pub again: &'static str,
}

pub async fn confirm(
    State(state): State<AppState>,
    session: Session,
    Path(app): Path<String>,
    OriginalUri(uri): OriginalUri,
    Query(q): Query<LinkQuery>,
) -> WebResult<Response> {
    let Some((app, (presses, again))) =
        LinkedApp::from_db_str(&app).and_then(|a| offer_copy(a).map(|copy| (a, copy)))
    else {
        return Ok(StatusCode::NOT_FOUND.into_response());
    };
    let Some(user) = session.user.clone() else {
        let back = uri.path_and_query().map_or("/", |p| p.as_str());
        return Ok(login_redirect(back).into_response());
    };
    let code = q.c.trim().to_string();
    let offered = state
        .linked_app_store
        .offered(app, &sha256_hex(&code))
        .await?;
    Ok(LinkAppPage {
        live: offered.is_some(),
        email: user.email,
        app: app.label(),
        slug: app.as_db_str(),
        account: offered.flatten(),
        code,
        presses,
        again,
    }
    .into_response())
}

/// What the page tells someone opening an offer for `app`. `None` for
/// Telegram, whose links the person asks for from their account page.
fn offer_copy(app: LinkedApp) -> Option<(&'static str, &'static str)> {
    match app {
        LinkedApp::Pushover => Some((
            "Emergency pages you acknowledge in Pushover",
            "Acknowledge an emergency page from Pushover and a fresh one follows, at most once a week.",
        )),
        LinkedApp::Slack => Some((
            "Alerts you acknowledge in Slack",
            "Press Acknowledge on a Slack alert again and a fresh one follows.",
        )),
        LinkedApp::Telegram => None,
    }
}
