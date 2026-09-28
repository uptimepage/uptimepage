//! `/link/pushover`, where the offer sent to a Pushover account that
//! acknowledged without a name lands. Signing in is what names the person, so
//! the page asks for a session first. It reads the offer only to say early that
//! it lapsed; the code is spent by the API call the page makes.

use askama::Template;
use askama_web::WebTemplate;
use axum::extract::{OriginalUri, Query, State};
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
#[template(path = "link_pushover.html")]
pub struct LinkPushoverPage {
    pub live: bool,
    pub email: String,
    /// The device that acknowledged, as Pushover named it.
    pub device: Option<String>,
    pub code: String,
}

pub async fn confirm(
    State(state): State<AppState>,
    session: Session,
    OriginalUri(uri): OriginalUri,
    Query(q): Query<LinkQuery>,
) -> WebResult<Response> {
    let Some(user) = session.user.clone() else {
        let back = uri
            .path_and_query()
            .map_or("/link/pushover", |p| p.as_str());
        return Ok(login_redirect(back).into_response());
    };
    let code = q.c.trim().to_string();
    let offered = state
        .linked_app_store
        .offered(LinkedApp::Pushover, &sha256_hex(&code))
        .await?;
    Ok(LinkPushoverPage {
        live: offered.is_some(),
        email: user.email,
        device: offered.flatten(),
        code,
    }
    .into_response())
}
